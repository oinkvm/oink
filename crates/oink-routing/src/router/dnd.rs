//! Router state machine for native drag handoffs between devices.
//!
//! 原始内容源保留原生会话；远端宿主独立开始/结束。原始物理 UP 与 StartDelivery
//! 共同授权最终源 Drop，回源只恢复原生会话。命令与回调绑定 offer/holder epoch。
//! 平台操作交给普通动作线程，路由不做 I/O。

use oink_foundation::content::Holder;
use std::collections::{HashMap, HashSet};

use crate::Packet as Message;
use crate::dnd::*;
#[cfg(test)]
use crate::input::InputMsg;
use oink_foundation::clipboard::ClipData;
use oink_foundation::content::ClipId;
use oink_foundation::content::OfferId;
use oink_foundation::dnd::DragImage;
use oink_foundation::input::MouseButton;
use oink_foundation::{DeviceId, Point};

use super::{Output, Router, Target};
use crate::damping::DampingDepth;
use crate::layout::ScreenPos;

mod cancel;
mod gesture;
use gesture::EdgeGesture;
mod integrity;
pub use integrity::{
    DndIntegrity, DndSourceEffect, IntegrityOutcome, NativeObservation, PhysicalObservation, RolePartition,
};
pub use source::Phase as SourcePhase;
#[cfg(test)]
mod model;
mod notice;
mod press;
mod source;
mod target;
pub use target::{DndPreparation, DndTarget};

/// 接收窗口边长的一半（本机单位）：光标离上次放窗口的位置超过它，就把窗口挪到光标下。
const PROBE_FOLLOW_UNITS: f64 = 12.0;

/// 交给平台执行的拖放操作（本机动作线程上按顺序执行，与注入、模式切换同一条队列）。
#[derive(Clone, Debug, PartialEq)]
pub enum DndCmd {
    Integrity {
        id: ClipId,
        record: DndIntegrity,
    },
    RevokePreparation {
        preparation: DndPreparation,
    },
    Prepare {
        preparation: DndPreparation,
        roots: Vec<oink_foundation::content::FileNode>,
    },
    ExportFailed {
        failure: oink_foundation::dnd::ExportFailure,
    },
    PrepareDrop {
        id: ClipId,
        epoch: u64,
    },
    HoldSource {
        origin: Option<oink_foundation::Point>,
        target: crate::config::Os,
        crossing: std::time::Instant,
        id: ClipId,
        epoch: u64,
        physical: bool,
    },
    /// Export the original held promise for a later Windows destination.
    ExportSource {
        id: ClipId,
        epoch: u64,
        physical: bool,
    },
    ResumeSource {
        id: ClipId,
        epoch: u64,
    },
    ReleaseSourceButton {
        id: ClipId,
        physical: bool,
    },
    FinishSource {
        id: ClipId,
        epoch: u64,
        cancel_at: Option<Point>,
        cancel_geometry: Option<oink_foundation::dnd::CancelGeometry>,
        accept: bool,
        physical: bool,
    },
    ForgetSource {
        id: ClipId,
    },
    ReleaseData {
        id: ClipId,
    },
    /// Set optional artwork for the immediately following Begin.
    SetImage(Option<DragImage>),
    /// Best effort update of the currently hosted drag; never starts a drag.
    UpdateImage {
        id: ClipId,
        epoch: u64,
        image: DragImage,
    },
    /// 在 `at`（本机全局坐标，光标所在的屏幕边缘）放出（或挪动）接收窗口，看有没有系统拖拽经过。
    /// 有就报告 [`DndEvent::Detected`]。
    Probe {
        at: Point,
    },
    /// 撤掉接收窗口。
    Unprobe,
    /// End this host for a handoff. The acknowledgement belongs to this epoch.
    LeaveHost {
        id: ClipId,
        epoch: u64,
        physical: bool,
    },
    /// 结束本机由 Oink 发起的拖拽，不放到任何程序里。
    Cancel {
        id: ClipId,
        epoch: u64,
        physical: bool,
    },
    /// 在 `at`（光标已放在这里）发起一次本地拖拽：放小窗、注入左键按下、发起系统拖拽。内容 `items`；
    /// 文件内容从 `id.origin` 拉取；返回内容源使用 ResumeSource。拖拽结束时报告 [`DndEvent::Ended`]。
    Begin {
        grant: DndTarget,
        id: ClipId,
        epoch: u64,
        at: Point,
        items: Vec<ClipData>,
        retained_inventory: Option<Vec<u8>>,
    },
}

/// 平台（经服务进程）报告给路由的拖放事件。
#[derive(Clone, Debug, PartialEq)]
pub enum DndEvent {
    /// Native UP submission failed; this does not claim the OS drag ended.
    HostReleaseFailed {
        id: ClipId,
        epoch: u64,
    },
    SourceEffectFailed {
        id: ClipId,
        epoch: u64,
        effect: DndSourceEffect,
    },
    SourceExported {
        id: ClipId,
        epoch: u64,
        items: Result<Vec<ClipData>, oink_foundation::dnd::ExportFailure>,
    },
    SourceHeld {
        id: ClipId,
        epoch: u64,
        ok: bool,
    },
    SourceCatcherLost {
        id: ClipId,
    },
    SourceButtonReleased {
        id: ClipId,
        ok: bool,
    },
    SourceEnded {
        id: ClipId,
        accepted: bool,
    },
    /// Optional artwork completed independently of metadata and crossing.
    SourceImage {
        id: ClipId,
        image: DragImage,
    },
    /// A source with real artwork; content semantics are unchanged.
    DetectedWithImage {
        id: ClipId,
        items: Vec<ClipData>,
        image: DragImage,
    },
    /// 接收窗口上有系统拖拽经过，内容是 `items`（文件清单带 `local_path`），已登记为 `id` 供对端拉取。
    Detected {
        id: ClipId,
        items: Vec<ClipData>,
    },
    /// This host's native drag has ended for a handoff.
    HostLeft {
        id: ClipId,
        epoch: u64,
        ok: bool,
    },
    /// `Begin` 发起的拖拽结束了（`dropped`：放到了某个目标上）；`Begin` 失败时也报告（`dropped = false`）。
    Ended {
        id: ClipId,
        epoch: u64,
        dropped: bool,
    },
    /// Retired after explicit cancel/release and catcher exit; no native end.
    SourceCancelRetired {
        id: ClipId,
    },
}

/// 宿主状态。
#[derive(Clone, Debug, Default)]
pub(super) enum State {
    #[default]
    Idle,
    /// 接收窗口已放出；`content` 为 `Some` 时平台已报告内容（设计里的 `Ready`）。
    Probing {
        input: DeviceId,
        gesture: EdgeGesture,
        content: Option<(ClipId, Vec<ClipData>)>,
    },
    /// 等平台确认源会话 hold（`native`）或结束旧宿主（`LeaveHost`）。
    Leaving {
        epoch: u64,
        input: DeviceId,
        gesture: EdgeGesture,
        id: ClipId,
        items: Vec<ClipData>,
        native: bool,
        /// 等待期间输入源松开了左键：不再移交。
        released: bool,
    },
    /// 本机在进行一次由 Oink 发起的拖拽。
    Hosting {
        id: ClipId,
        input: DeviceId,
        items: Vec<ClipData>,
        push: Option<EdgeGesture>,
    },
}

/// 收到、还没配上 `FocusEnter` 的 `DndMsg::Enter`。
#[derive(Clone, Debug)]
pub(super) struct PendingEnter {
    from: DeviceId,
    id: ClipId,
    epoch: u64,
    input: DeviceId,
    items: Vec<ClipData>,
    image: Option<DragImage>,
    preview_ready: bool,
    preview_binding: Option<PreviewBinding>,
    retained_inventory: Option<Vec<u8>>,
    incomplete: bool,
    focus_entered: bool,
    grant: Option<(DragKey, u64, u64)>,
}

/// 要随下一次移交发出的 `DndMsg::Enter`（`epoch` 在 `hand_off` 里填）。
#[derive(Clone, Debug)]
pub(super) struct Outgoing {
    to: DeviceId,
    id: ClipId,
}

type SourceActionAt = (SourceAction, Option<Point>);

#[derive(Debug, Default)]
pub(super) struct Dnd {
    sources: HashMap<OfferId, source::Source>,
    pending_integrity: HashMap<OfferId, DndIntegrity>,
    presses: HashMap<OfferId, press::OriginalPress>,
    notices: HashMap<DeviceId, notice::Received>,
    preparations: HashMap<DeviceId, DndPreparation>,
    target: Option<DndTarget>,
    deliveries: HashMap<DragKey, DndTarget>,
    source_sessions: HashMap<DeviceId, u64>,
    next_source_session: u64,
    refused: HashMap<DeviceId, (ClipId, u64)>,
    input_notices: HashMap<DeviceId, (DragKey, u64, SourceButtonDisposition)>,
    native_source: Option<OfferId>,
    pending_source: HashMap<(OfferId, Holder), Vec<SourceActionAt>>,
    pub(super) now: u64,
    /// 本机平台能做跨设备拖放（服务进程设置）。
    available: bool,
    /// 发过 `Caps { enabled: true }` 的对端。
    peers: HashSet<DeviceId>,
    pub(super) state: State,
    pending: Option<PendingEnter>,
    last_enter_epoch: Option<u64>,
    image: Option<(ClipId, DragImage)>,
    pub(super) press_origin: Option<ScreenPos>,
    preview_requested: Option<(DeviceId, ClipId, u64, PreviewBinding)>,
    crossing_evidence: Option<(ScreenPos, (f64, f64), bool)>,
    pub(super) outgoing: Option<Outgoing>,
}

impl Router {
    fn dnd_offer(id: ClipId) -> OfferId {
        OfferId {
            clip: id,
            generation: id.seq,
        }
    }

    fn dnd_cancel_geometry(
        &self,
        offer: OfferId,
        remote: Option<Point>,
    ) -> Option<oink_foundation::dnd::CancelGeometry> {
        if self.cfg.os != crate::config::Os::MacOS || self.holder == self.me() {
            return None;
        }
        let source = self.dnd.sources.get(&offer)?;
        let projected = source
            .origin
            .as_ref()
            .and_then(|origin| self.layout.from_plane_unclamped(&origin.screen, remote?));
        let edge = source.origin.as_ref().and_then(|origin| {
            let displays: Vec<_> = self.displays.iter().map(|d| d.bounds).collect();
            cancel::last_exit(origin.local, projected?, &displays)
        });
        Some(oink_foundation::dnd::CancelGeometry {
            down: source.origin.as_ref().map(|p| p.local),
            target_plane: remote,
            projected,
            edge,
            fallback: edge.is_none().then_some(source.exit).flatten(),
        })
    }

    /// Bulk demand is authorized by the same holder generation as I0 controls.
    /// The caller waits outside the input/native queues; native Drop may proceed
    /// before the target's synchronous GetData has returned.
    pub fn on_dnd_content_demand(&mut self, from: DeviceId, offer: OfferId, holder: Holder) -> bool {
        let valid = self.on_dnd_namespace_demand(from, offer, holder);
        if valid {
            self.dnd_source_action(from, offer, holder, SourceAction::StartDelivery, None);
        }
        valid
    }
    /// Enumeration authenticates the same holder but never authorizes native
    /// Drop. OLE can ask for descriptors while still hovering over a target.
    pub fn on_dnd_namespace_demand(&self, from: DeviceId, offer: OfferId, holder: Holder) -> bool {
        self.dnd.sources.get(&offer).is_some_and(|source| {
            source.matches(offer, holder, from)
                && (source.exported && source.phase == source::Phase::Local
                    || matches!(
                        source.phase,
                        source::Phase::Held
                            | source::Phase::Released
                            | source::Phase::Committing
                            | source::Phase::Receiving
                    ))
        })
    }
    fn dnd_source_action(
        &mut self,
        from: DeviceId,
        offer: OfferId,
        holder: Holder,
        action: SourceAction,
        cancel_at: Option<Point>,
    ) {
        let Some(source) = self.dnd.sources.get_mut(&offer) else {
            if from == holder.device {
                self.send(
                    from,
                    Message::Dnd(DndMsg::Refused {
                        id: offer.clip,
                        holder,
                        reason: RevokeReason::SourceUnavailable,
                    }),
                );
            }
            return;
        };
        if !source.matches(offer, holder, from) {
            // A new host's bulk/drop request may beat its predecessor's ordered
            // handoff to C. Buffer a small amount of intent, never authority.
            if from == holder.device
                && holder.epoch > source.holder.epoch
                && source.phase == source::Phase::Held
                && self.dnd.pending_source.len() < 4
            {
                let pending = self.dnd.pending_source.entry((offer, holder)).or_default();
                if pending.iter().any(|(a, _)| *a == action) {
                    return;
                }
                if pending.len() < 4 {
                    pending.push((action, cancel_at));
                    return;
                }
            }
            if from == holder.device {
                self.send(
                    from,
                    Message::Dnd(DndMsg::Refused {
                        id: offer.clip,
                        holder,
                        reason: RevokeReason::InvalidTransition,
                    }),
                );
            }
            return;
        }
        let mut commit = false;
        let mut cancel = false;
        let mut handoff = None;
        match action {
            SourceAction::Handoff { next } => {
                if source.handoff(holder, next) {
                    handoff = Some(next);
                }
            }
            SourceAction::StartDelivery => commit = source.demanded(),
            SourceAction::Cancel => cancel = source.cancel(),
        }
        let exported = source.exported;
        let closed = source.phase == source::Phase::Closed;
        if (commit && !exported) || cancel {
            let at = cancel.then(|| self.dnd_cancel_geometry(offer, cancel_at)).flatten();
            self.dnd_submit_source_finish(offer, commit, at);
        }
        if cancel || closed {
            self.dnd_publish_state(
                offer,
                DragState::Revoked {
                    scope: RevokeScope::Drag,
                    reason: RevokeReason::Cancelled,
                },
            );
        }
        if closed {
            self.dnd_integrity_end(offer, IntegrityOutcome::Cancelled);
            self.dnd.sources.remove(&offer);
            self.dnd.pending_source.retain(|(id, _), _| *id != offer);
            self.out.push(Output::Dnd(DndCmd::ReleaseData { id: offer.clip }));
        }
        if let Some(next) = handoff {
            let windows = self
                .peers
                .get(&next.device)
                .is_some_and(|peer| peer.os == crate::Os::Windows);
            if let Some(source) = self.dnd.sources.get_mut(&offer)
                && windows
                && !source.exported
                && source.exporting.is_none()
                && source.items.iter().any(|item| {
                    matches!(item,
                    ClipData::Native { platform: oink_foundation::clipboard::ClipPlatform::Mac, item }
                    if item.file.is_none() && item.representations.iter().any(|rep|
                        matches!(rep.data, oink_foundation::content::RepresentationData::Promise(..))))
                })
            {
                // First-hop Mac hosts keep native promises. A later Windows
                // holder needs the same source export as a direct departure,
                // before it receives an offer with FileGroupDescriptor names.
                source.exporting = Some(next.epoch);
                self.out.push(Output::Dnd(DndCmd::ExportSource {
                    id: offer.clip,
                    epoch: next.epoch,
                    physical: source.input == self.cfg.device,
                }));
            }
            self.dnd_announce_members(offer);
            self.dnd_publish_offer(offer);
            let actions = self.dnd.pending_source.remove(&(offer, next)).unwrap_or_default();
            self.dnd
                .pending_source
                .retain(|(id, h), _| *id != offer || h.epoch > next.epoch);
            for (action, cancel_at) in actions {
                self.dnd_source_action(next.device, offer, next, action, cancel_at);
            }
        }
    }

    fn dnd_notify_source(&mut self, id: ClipId, action: SourceAction) {
        self.dnd_notify_source_at(
            id,
            Holder {
                device: self.me(),
                epoch: self.epoch,
            },
            action,
        );
    }

    fn dnd_notify_source_at(&mut self, id: ClipId, holder: Holder, action: SourceAction) {
        let offer = Self::dnd_offer(id);
        let cancel_at = (action == SourceAction::Cancel && self.holder == self.me())
            .then(|| self.pointer.as_ref().and_then(|p| self.layout.to_plane(p)))
            .flatten();
        if id.origin == self.me() {
            self.dnd_source_action(self.me(), offer, holder, action, cancel_at);
        } else {
            self.send(
                id.origin,
                Message::Dnd(DndMsg::Source {
                    offer,
                    holder,
                    action,
                    cancel_at,
                }),
            );
        }
    }

    fn dnd_source_ended(&mut self, id: ClipId, observation: NativeObservation) {
        let offer = Self::dnd_offer(id);
        let Some(source) = self.dnd.sources.get_mut(&offer) else {
            return;
        };
        if self.dnd.native_source != Some(offer) {
            return;
        }
        let catcher_lost = observation == NativeObservation::CatcherLost;
        let cancel_retired = observation == NativeObservation::CancelRetired;
        if cancel_retired && source.phase != source::Phase::Cancelling {
            return;
        }
        let accepted = observation == NativeObservation::EndedAccepted && source.phase == source::Phase::Committing;
        if catcher_lost {
            source.catcher_lost();
        } else if cancel_retired {
            source.cancel_retired();
        } else {
            source.native_ended(accepted);
        }
        let terminal = source.phase == source::Phase::Closed;
        self.dnd.native_source = None;
        self.dnd_return_source_press(offer);
        if terminal {
            self.dnd_publish_state(
                offer,
                if catcher_lost || cancel_retired {
                    DragState::Revoked {
                        scope: RevokeScope::Drag,
                        reason: if catcher_lost {
                            RevokeReason::CatcherLost
                        } else {
                            RevokeReason::Cancelled
                        },
                    }
                } else {
                    DragState::Ended { accepted }
                },
            );
        }
        if matches!(self.dnd.state, State::Leaving { id: active, native: true, .. } if active == id) {
            self.dnd.state = State::Idle;
        }
        self.out.push(Output::Dnd(DndCmd::ForgetSource { id }));
        if terminal {
            self.dnd_integrity_end(
                offer,
                if catcher_lost {
                    IntegrityOutcome::CatcherLost
                } else if accepted {
                    IntegrityOutcome::Completed
                } else {
                    IntegrityOutcome::Cancelled
                },
            );
            self.dnd.sources.remove(&offer);
            self.dnd.pending_source.retain(|(id, _), _| *id != offer);
            self.out.push(Output::Dnd(DndCmd::ReleaseData { id }));
        }
    }

    /// File service terminal receipt: independent of the group's current focus.
    pub fn on_dnd_delivery_receipt(
        &mut self,
        from: DeviceId,
        delivery: oink_foundation::content::DeliveryId,
        success: bool,
    ) -> bool {
        let offer = delivery.offer;
        let Some(source) = self.dnd.sources.get_mut(&offer).filter(|s| {
            delivery.receiver == from
                && s.holder.device == from
                && s.holder.epoch == delivery.sequence
                && matches!(s.phase, source::Phase::Committing | source::Phase::Receiving)
        }) else {
            return false;
        };
        if success {
            if !source.target_stored() {
                return false;
            }
            if source.phase == source::Phase::Committing {
                // Cached hover text can be stored before the original native
                // Drop finishes snapshotting. ReleaseData cancels that provider,
                // and an Ended notice cancels its still-running remote host.
                // Join the receipt with native completion before either cleanup;
                // exported sources already have a closed native session.
                source.advance();
                return true;
            }
        } else if source.cancel() {
            self.dnd_submit_source_finish(offer, false, self.dnd_cancel_geometry(offer, None));
        }
        if let Some(source) = self.dnd.sources.get_mut(&offer) {
            source.advance();
        }
        self.dnd_publish_state(offer, DragState::Ended { accepted: success });
        self.out.push(Output::Dnd(DndCmd::ReleaseData { id: offer.clip }));
        self.dnd_integrity_end(
            offer,
            if success {
                IntegrityOutcome::Completed
            } else {
                IntegrityOutcome::Cancelled
            },
        );
        if self.dnd.native_source != Some(offer) {
            self.dnd.sources.remove(&offer);
        }
        true
    }
    fn dnd_record_refusal(&mut self, id: ClipId, epoch: u64) {
        // At most one terminal expectation per admitted source. Unknown sources
        // beyond the same notice capacity cannot obtain a Begin grant either.
        if self.dnd.refused.len() >= 16 && !self.dnd.refused.contains_key(&id.origin) {
            return;
        }
        if self
            .dnd
            .refused
            .get(&id.origin)
            .is_none_or(|(old, seen)| id.seq > old.seq || (id == *old && epoch > *seen))
        {
            self.dnd.refused.insert(id.origin, (id, epoch));
        }
    }
    fn dnd_cancel_pending(&mut self) {
        if let Some(p) = self.dnd.pending.take() {
            self.dnd_record_refusal(p.id, p.epoch);
            if p.id.origin != self.me() {
                self.send(
                    p.id.origin,
                    Message::Dnd(DndMsg::Source {
                        offer: Self::dnd_offer(p.id),
                        holder: Holder {
                            device: self.me(),
                            epoch: p.epoch,
                        },
                        action: SourceAction::Cancel,
                        cancel_at: None,
                    }),
                );
            }
        }
        self.dnd.preview_requested = None;
    }

    pub fn on_dnd_refused(&mut self, from: DeviceId, id: ClipId, epoch: u64) {
        if self
            .dnd
            .pending
            .as_ref()
            .is_some_and(|p| (p.from == from || p.id.origin == from) && p.id == id && p.epoch == epoch)
        {
            self.dnd_cancel_pending();
        }
        if from == id.origin
            && self.dnd.target.as_ref().is_some_and(|g| {
                g.preparation.key.drag.offer.clip == id && g.holder.epoch == epoch && g.begin_current()
            })
        {
            self.dnd_cancel_hosting();
        }
    }
    // Pairing is bounded by one focus expectation and one declaration per
    // admitted source. Only explicit refusal/revocation ends an expectation.
    pub(super) fn dnd_deadline(&self) -> Option<u64> {
        None
    }
    pub(super) fn dnd_expire(&mut self) {}

    // ================= 公共接口 =================

    /// 本机平台能不能做跨设备拖放（服务进程在平台拖放启动后设置）。变化时通告各对端。
    pub fn set_dnd_available(&mut self, available: bool) {
        if self.dnd.available == available {
            return;
        }
        self.dnd.available = available;
        if !available {
            self.dnd_abort("drag and drop unavailable");
        }
        self.broadcast_dnd_caps();
    }

    /// 平台报告的拖放事件。
    pub fn on_dnd_event(&mut self, ev: DndEvent) {
        if let DndEvent::SourceExported { id, epoch, items } = ev {
            let offer = Self::dnd_offer(id);
            // An export callback owns only the native source it started for.
            // Duplicate completion must not revoke an already published offer.
            if self.dnd.native_source != Some(offer) {
                return;
            }
            let initial = matches!(self.dnd.state, State::Leaving { id: active, epoch: active_epoch, native: true, .. } if active == id && active_epoch == epoch);
            let later = self
                .dnd
                .sources
                .get(&offer)
                .is_some_and(|source| source.exporting == Some(epoch));
            let current = initial || later;
            let ready = current
                && !self.fully_paused
                && items.is_ok()
                && self
                    .dnd
                    .sources
                    .get_mut(&offer)
                    .is_some_and(source::Source::export_ready);
            if self.dnd.native_source == Some(offer) {
                self.dnd.native_source = None;
            }
            self.out.push(Output::Dnd(DndCmd::ForgetSource { id }));
            if ready {
                if later {
                    self.dnd.sources.get_mut(&offer).unwrap().metadata(items.unwrap());
                    self.dnd_announce_members(offer);
                    self.dnd_publish_offer(offer);
                } else {
                    if let State::Leaving { items: pending, .. } = &mut self.dnd.state {
                        *pending = items.unwrap();
                    }
                    self.dnd_released(true);
                }
            } else {
                if current
                    && !self.fully_paused
                    && (later || matches!(self.dnd.state, State::Leaving { released: false, .. }))
                    && let Err(failure) = items
                    && failure != oink_foundation::dnd::ExportFailure::Cancelled
                {
                    self.out.push(Output::Dnd(DndCmd::ExportFailed { failure }));
                }
                if initial {
                    self.dnd.state = State::Idle;
                }
                if let Some(source) = self.dnd.sources.get_mut(&offer) {
                    source.advance();
                    source.phase = source::Phase::Closed;
                    source.native = NativeObservation::Unconfirmed(DndSourceEffect::Release);
                }
                self.dnd_return_source_press(offer);
                self.dnd_publish_state(
                    offer,
                    DragState::Revoked {
                        scope: RevokeScope::Drag,
                        reason: RevokeReason::PreparationFailed,
                    },
                );
                self.dnd_integrity_end(offer, IntegrityOutcome::ExportFailed);
                self.dnd.sources.remove(&offer);
                self.dnd.pending_source.retain(|(id, _), _| *id != offer);
                self.out.push(Output::Dnd(DndCmd::ReleaseData { id }));
            }
            return;
        }
        if let DndEvent::SourceEffectFailed { id, epoch, effect } = ev {
            let offer = Self::dnd_offer(id);
            if let Some(source) = self.dnd.sources.get_mut(&offer).filter(|s| s.holder.epoch == epoch) {
                source.native = NativeObservation::Unconfirmed(effect);
                source.advance();
                source.phase = source::Phase::Closed;
                self.dnd_return_source_press(offer);
                self.dnd_publish_state(
                    offer,
                    DragState::Revoked {
                        scope: RevokeScope::Drag,
                        reason: RevokeReason::SourceUnavailable,
                    },
                );
                self.dnd_integrity_end(offer, IntegrityOutcome::EffectUnconfirmed);
                self.dnd.sources.remove(&offer);
                self.dnd.pending_source.retain(|(id, _), _| *id != offer);
                if matches!(self.dnd.state, State::Leaving { id: active, native: true, .. } if active == id) {
                    self.dnd.state = State::Idle;
                }
                self.dnd.native_source = self.dnd.native_source.filter(|s| *s != offer);
                self.out.push(Output::Dnd(DndCmd::ForgetSource { id }));
                self.out.push(Output::Dnd(DndCmd::ReleaseData { id }));
            }
            return;
        }
        if let DndEvent::SourceCancelRetired { id } = ev {
            self.dnd_source_ended(id, NativeObservation::CancelRetired);
            return;
        }
        if let DndEvent::SourceCatcherLost { id } = ev {
            self.dnd_source_ended(id, NativeObservation::CatcherLost);
            return;
        }
        if let DndEvent::SourceButtonReleased { id, ok } = ev {
            let offer = Self::dnd_offer(id);
            let input_lost = self
                .dnd
                .presses
                .get(&offer)
                .is_some_and(|p| p.input != self.me() && !self.reachable(p.input));
            if let Some(press) = self.dnd.presses.get_mut(&offer) {
                press.submit();
                press.receipt(ok);
                if input_lost {
                    press.retire_input_route();
                }
            }
            self.dnd_button_notice(offer);
            self.dnd_integrity_receipt(offer);
            if self.dnd.presses.get(&offer).is_some_and(press::OriginalPress::retired) {
                self.dnd.presses.remove(&offer);
            }
            return;
        }
        if let DndEvent::SourceEnded { id, accepted } = ev {
            self.dnd_source_ended(
                id,
                if accepted {
                    NativeObservation::EndedAccepted
                } else {
                    NativeObservation::EndedRejected
                },
            );
            return;
        }
        if let DndEvent::SourceHeld { id, epoch, ok } = ev {
            if matches!(&self.dnd.state, State::Leaving { id: active, epoch: active_epoch, native: true, .. } if *active == id && *active_epoch == epoch)
            {
                self.dnd_released(ok && self.dnd.native_source == Some(Self::dnd_offer(id)));
            }
            return;
        }
        if let DndEvent::HostReleaseFailed { id, epoch } = ev {
            if matches!(self.dnd.state, State::Hosting { id: active, .. } if active == id) && self.epoch == epoch {
                self.dnd_cancel_hosting();
            }
            return;
        }
        if self.fully_paused {
            return;
        }
        tracing::debug!(?ev, state = ?self.dnd_state_name(), "dnd event");
        match ev {
            DndEvent::SourceEnded { .. }
            | DndEvent::SourceHeld { .. }
            | DndEvent::SourceEffectFailed { .. }
            | DndEvent::SourceExported { .. }
            | DndEvent::SourceCatcherLost { .. }
            | DndEvent::SourceCancelRetired { .. }
            | DndEvent::SourceButtonReleased { .. }
            | DndEvent::HostReleaseFailed { .. } => unreachable!(),
            DndEvent::SourceImage { id, image } => {
                let offer = Self::dnd_offer(id);
                if id.origin == self.me()
                    && image.valid()
                    && self
                        .dnd
                        .sources
                        .get(&offer)
                        .is_some_and(|s| matches!(s.phase, source::Phase::Local | source::Phase::Held))
                {
                    self.dnd.image = Some((id, image));
                    self.dnd_publish_offer(offer);
                }
            }
            DndEvent::Detected { id, items } => {
                self.dnd.image = None;
                self.dnd_detected(id, items);
            }
            DndEvent::DetectedWithImage { id, items, image } => {
                self.dnd.image = image.valid().then_some((id, image));
                self.dnd_detected(id, items);
            }
            DndEvent::HostLeft { id, epoch, ok } => {
                if matches!(self.dnd.state, State::Leaving { id: active, epoch: active_epoch, native: false, .. } if active == id && active_epoch == epoch)
                {
                    self.dnd_released(ok);
                }
            }
            DndEvent::Ended { id, epoch, dropped } => {
                if let State::Hosting { id: active, input, .. } = &self.dnd.state
                    && *active == id
                    && self
                        .dnd
                        .target
                        .as_ref()
                        .map_or(self.epoch == epoch, |g| g.holder.epoch == epoch)
                {
                    let input = *input;
                    tracing::info!(?id, dropped, "drag ended on this device");
                    if let Some(grant) = self.dnd.target.take() {
                        if dropped {
                            grant.delivered();
                            self.dnd.deliveries.insert(grant.preparation.key.drag, grant);
                        } else {
                            grant.revoke();
                        }
                    }
                    if dropped {
                        self.dnd.preparations.remove(&id.origin);
                    } else {
                        self.dnd_retire_preparation(id.origin);
                    }
                    self.dnd.state = State::Idle;
                    self.release_button_silently(input, MouseButton::Left);
                    self.dnd_notify_source_at(
                        id,
                        Holder {
                            device: self.me(),
                            epoch,
                        },
                        if dropped {
                            SourceAction::StartDelivery
                        } else {
                            SourceAction::Cancel
                        },
                    );
                }
            }
        }
    }

    /// Ordinary declaration preparation failed before native handoff.
    pub fn cancel_dnd(&mut self) {
        if !self.dnd_drop_released() {
            self.dnd_abort("drag declaration unavailable");
        }
    }

    /// 当前的拖放状态（诊断、测试）。
    pub fn dnd_state_name(&self) -> &'static str {
        match self.dnd.state {
            State::Idle => "idle",
            State::Probing { content: None, .. } => "probing",
            State::Probing { content: Some(_), .. } => "ready",
            State::Leaving { .. } => "leaving",
            State::Hosting { .. } => "hosting",
        }
    }

    /// A target may prepare native file URLs only while this exact host still
    /// owns the held button. Releasing during preparation must not create a late
    /// native drag at a position that the user has already left.
    pub fn dnd_host_pending(&self) -> Option<(ClipId, u64)> {
        let State::Hosting { id, input, .. } = self.dnd.state else {
            return None;
        };
        (self.dnd.target.as_ref().is_some_and(DndTarget::begin_current)
            && self.holder == self.me()
            && self
                .held
                .get(&input)
                .is_some_and(|held| held.buttons.contains(&MouseButton::Left)))
        .then_some((id, self.epoch))
    }

    // ================= 路由内的钩子 =================

    /// 正在等平台结束本机的拖拽、准备移交（这期间通往其他设备的边仍是墙）。
    pub(super) fn dnd_leaving(&self) -> bool {
        matches!(self.dnd.state, State::Leaving { .. })
    }

    pub(super) fn dnd_enabled(&self) -> bool {
        self.dnd.available && self.cfg.settings.dnd
    }

    pub(super) fn broadcast_dnd_caps(&mut self) {
        let enabled = self.dnd_enabled();
        self.broadcast(Message::Dnd(DndMsg::Caps { enabled }));
    }

    pub(super) fn on_dnd_msg(&mut self, from: DeviceId, msg: DndMsg) {
        if self.layout.source_requires_target(self.me()) && matches!(msg, DndMsg::Enter { .. }) {
            return;
        }
        match msg {
            DndMsg::Source {
                offer,
                holder,
                action,
                cancel_at,
            } => self.dnd_source_action(from, offer, holder, action, cancel_at),
            DndMsg::PreviewRequest { id, epoch, binding } => {
                // Production Low requests use DndGlue's local source registry,
                // never an image cached by a previous stand-in host. Keep this
                // routing-only path (also used by Sim) subject to that authority.
                let local = id.origin == self.me()
                    && self.dnd.sources.get(&binding.key.offer).is_some_and(|source| {
                        source.key() == binding.key
                            && source.metadata_revision == binding.metadata_revision
                            && self.dnd_preview_availability(id)
                                == PreviewAvailability::Available {
                                    revision: binding.preview_revision,
                                }
                    });
                let image = self
                    .dnd
                    .image
                    .clone()
                    .filter(|(found, _)| local && *found == id)
                    .map(|(_, image)| image);
                self.send(
                    from,
                    Message::Dnd(DndMsg::Preview {
                        id,
                        epoch,
                        binding,
                        image,
                    }),
                );
            }
            DndMsg::Preview {
                id,
                epoch,
                binding,
                image,
            } => {
                if self.dnd.preview_requested == Some((from, id, epoch, binding))
                    && image.as_ref().is_none_or(DragImage::valid)
                    && let Some(pending) = self.dnd.pending.as_mut().filter(|p| {
                        p.id.origin == from && p.id == id && p.epoch == epoch && p.preview_binding == Some(binding)
                    })
                {
                    pending.image = image;
                    pending.preview_ready = true;
                    if pending.focus_entered && !pending.incomplete {
                        let handoff_from = pending.from;
                        let pos = self.pointer.as_ref().map(|p| p.local).unwrap_or_default();
                        self.dnd_focus_entered(handoff_from, epoch, pos);
                    }
                } else if self.dnd.preview_requested == Some((from, id, epoch, binding))
                    && from == id.origin
                    && let Some(image) = image.filter(DragImage::valid)
                    && self.dnd.target.as_ref().is_some_and(|g| {
                        g.holder.epoch == epoch
                            && g.begin_current()
                            && g.preparation.key.drag == binding.key
                            && g.preparation.key.metadata_revision == binding.metadata_revision
                    })
                    && matches!(self.dnd.state, State::Hosting { id: active, .. } if active == id)
                {
                    self.out.push(Output::Dnd(DndCmd::UpdateImage { id, epoch, image }));
                }
            }
            DndMsg::Caps { enabled } => {
                tracing::debug!(?from, enabled, "peer drag-and-drop capability");
                if enabled {
                    self.dnd.peers.insert(from);
                } else {
                    self.dnd.peers.remove(&from);
                    self.dnd_peer_lost(from);
                }
            }
            DndMsg::Notice(notice) => self.on_dnd_notice(from, notice, false, self.dnd.now),
            DndMsg::NoticeRequest { id, holder, previous } => self.dnd_notice_request(from, id, holder, previous),
            DndMsg::Refused { id, holder, .. } => {
                if from == id.origin && holder.device == self.me() {
                    self.dnd_record_refusal(id, holder.epoch);
                    self.on_dnd_refused(from, id, holder.epoch);
                }
            }
            DndMsg::SourceButton {
                key,
                revision,
                disposition,
            } => {
                if from == key.offer.clip.origin
                    && self.dnd.input_notices.get(&from).is_none_or(|(old, seen, _)| {
                        key.native_generation > old.native_generation || (key == *old && revision >= *seen)
                    })
                {
                    // Receipt is observational: it never changes the original
                    // ordinary DOWN route or consumes a later physical press.
                    self.dnd.input_notices.insert(from, (key, revision, disposition));
                }
            }
            DndMsg::Enter { id, epoch } => {
                if epoch < self.epoch
                    || self.dnd.last_enter_epoch.is_some_and(|seen| seen >= epoch)
                    || self
                        .dnd
                        .refused
                        .get(&id.origin)
                        .is_some_and(|(old, seen)| id.seq < old.seq || (id == *old && epoch <= *seen))
                {
                    return;
                }
                self.dnd.last_enter_epoch = Some(epoch);
                self.dnd_cancel_pending();
                self.dnd.pending = Some(PendingEnter {
                    from,
                    id,
                    epoch,
                    input: id.origin,
                    items: Vec::new(),
                    incomplete: true,
                    focus_entered: false,
                    image: None,
                    preview_ready: false,
                    preview_binding: None,
                    retained_inventory: None,
                    grant: None,
                });
                self.dnd_pair_notice();
                if id.origin != self.me() {
                    self.send(
                        id.origin,
                        Message::Dnd(DndMsg::NoticeRequest {
                            id,
                            holder: Holder {
                                device: self.me(),
                                epoch,
                            },
                            previous: from,
                        }),
                    );
                }
            }
        }
    }

    /// 本机指针（焦点机）的一次移动贴着通往 `dst` 的边、往外推，而那条边因为按钮按着是墙
    /// （`handoff_blocked`）。`input`：这次移动的来源（按着左键的设备）；`from`：移动前的位置；
    /// `delta`：这一步的位移（plane）；`at`：光标（本机坐标）。
    pub(super) fn dnd_push(
        &mut self,
        input: DeviceId,
        from: &ScreenPos,
        dst: ScreenPos,
        delta_units: (f64, f64),
        at: Point,
    ) {
        if !self.dnd_enabled()
            || self.focus_pinned()
            || self.system_clip_active()
            || self.paused
            || self.dnd.pending.is_some()
        {
            return;
        }
        let to = dst.screen.device;
        if self.layout.reference_per_native_unit(&dst.screen).is_none()
            || !self.input_ready(to)
            || !self.dnd.peers.contains(&to)
            || !self.left_only(input)
        {
            self.dnd_off_edge();
            return;
        }
        let Some(display) = self.displays.iter().find(|d| d.key == from.screen.display) else {
            return;
        };
        let diagonal = display.bounds.w.hypot(display.bounds.h);
        let Some(edge) = self.exit_edge(from, delta_units) else {
            return;
        };
        let out = (delta_units.0 * edge.normal.0 + delta_units.1 * edge.normal.1).max(0.0) / diagonal;
        let threshold = self.cfg.settings.dnd_push_fraction;
        if self.diagnostics.as_ref().is_some_and(|d| d.enabled()) {
            self.dnd.crossing_evidence = Some((
                from.clone(),
                delta_units,
                self.crossing_uses_zone(from, delta_units, true),
            ));
        }
        match &mut self.dnd.state {
            State::Idle => {
                let mut gesture = EdgeGesture::new(edge, dst, at);
                gesture.push(out, threshold);
                self.dnd.state = State::Probing {
                    input,
                    gesture,
                    content: None,
                };
                self.out.push(Output::Dnd(DndCmd::Probe { at }));
            }
            State::Probing {
                input: i,
                gesture,
                content,
            } => {
                if *i != input {
                    return;
                }
                gesture.select(edge, dst, at);
                gesture.push(out, threshold);
                if gesture.follow_probe() {
                    self.out.push(Output::Dnd(DndCmd::Probe { at }));
                }
                if content.is_some() && gesture.crossed(threshold) {
                    self.dnd_leave(true);
                }
            }
            State::Hosting { input: i, push, .. } => {
                if *i != input {
                    return;
                }
                let gesture = push.get_or_insert_with(|| EdgeGesture::new(edge.clone(), dst.clone(), at));
                gesture.select(edge, dst, at);
                gesture.push(out, threshold);
                if gesture.crossed(threshold) {
                    self.dnd_leave(false);
                }
            }
            State::Leaving { gesture, .. } => {
                if gesture.same_edge(&edge, &dst) {
                    gesture.follow(at, dst);
                }
            }
        }
    }

    /// Native probes and hosted drags share the same push gesture. Windows clamps
    /// at the last pixel: a tangent/zero report there is not an inward retreat.
    /// Keep depth only on the same edge toward the same destination, without
    /// counting tangential distance. Hosting must follow this rule too: otherwise
    /// its depth resets on every tangent (Windows 11 build-3 trace), while an
    /// identical native source push succeeds. This does not alter the threshold.
    pub(super) fn dnd_follow_edge(&mut self, origin: DeviceId, current: &ScreenPos, delta: (f64, f64)) -> bool {
        let (input, gesture) = match &self.dnd.state {
            State::Probing { input, gesture, .. } | State::Leaving { input, gesture, .. } => (input, gesture),
            State::Hosting {
                input,
                push: Some(gesture),
                ..
            } => (input, gesture),
            _ => return false,
        };
        if *input != origin || current.screen != gesture.edge.source {
            return false;
        }
        let n = gesture.edge.normal;
        let Some(display) = self.displays.iter().find(|d| d.key == current.screen.display) else {
            return false;
        };
        if !gesture.edge.contains(display.bounds, current.local) || delta.0 * n.0 + delta.1 * n.1 != 0. {
            return false;
        }
        let outward = (n.0 * (super::EDGE_UNITS + 1.), n.1 * (super::EDGE_UNITS + 1.));
        let Some(next) = self.layout.exit_through_edge(current, outward) else {
            return false;
        };
        let Some(edge) = self.exit_edge(current, outward) else {
            return false;
        };
        if !gesture.same_edge(&edge, &next) {
            return false;
        }
        let probing = matches!(self.dnd.state, State::Probing { .. });
        let gesture = match &mut self.dnd.state {
            State::Probing { gesture, .. } | State::Leaving { gesture, .. } => gesture,
            State::Hosting {
                push: Some(gesture), ..
            } => gesture,
            _ => unreachable!(),
        };
        gesture.follow(current.local, next);
        if probing && gesture.follow_probe() {
            self.out.push(Output::Dnd(DndCmd::Probe { at: current.local }));
        }
        true
    }

    /// The pointer left the probed edge or moved inward: clear the probe/depth.
    pub(super) fn dnd_off_edge(&mut self) {
        match &mut self.dnd.state {
            State::Probing { .. } => self.dnd_unprobe(),
            State::Hosting { push, .. } => *push = None,
            _ => {}
        }
    }

    /// 输入源（`input`）的左键抬起了（本机物理抬起，或远端源的 `Button{Left, up}` 到达）。
    pub(super) fn dnd_left_released(&mut self, input: DeviceId) {
        match &mut self.dnd.state {
            State::Probing { input: i, .. } if *i == input => self.dnd_unprobe(),
            State::Leaving { input: i, released, .. } if *i == input => {
                tracing::info!(
                    ?input,
                    "left button released while the drag was being handed over: staying here"
                );
                *released = true;
            }
            _ => {}
        }
    }

    pub(super) fn dnd_drop_released(&self) -> bool {
        self.dnd.target.as_ref().is_some_and(DndTarget::drop_pending)
            || self
                .dnd
                .native_source
                .and_then(|offer| self.dnd.sources.get(&offer))
                .is_some_and(|source| {
                    matches!(
                        source.phase,
                        source::Phase::Released | source::Phase::Committing | source::Phase::Receiving
                    )
                })
    }

    /// Escape cancels only while the original pointer button is still held.
    pub(super) fn dnd_escape(&mut self, input: DeviceId) -> bool {
        // Native completion is asynchronous on macOS (observed with QuickTime
        // on macOS 14). Physical release has already committed the user's drop;
        // swallow Esc so neither the hosted drag nor the retained original
        // source can turn a later key into native cancellation.
        if self.dnd_drop_released() {
            return true;
        }
        if self.dnd.pending.as_ref().is_some_and(|p| p.input == input) {
            self.dnd_cancel_pending();
            return true;
        }
        match &self.dnd.state {
            State::Hosting { input: i, .. } if *i == input => {
                tracing::info!(?input, "Esc during a drag: cancelling it");
                self.dnd_cancel_hosting();
                true
            }
            State::Probing { input: i, .. } if *i == input => {
                // 系统自己取消源拖拽（Esc 照常交给系统）。
                self.dnd_unprobe();
                false
            }
            State::Leaving { input: i, .. } if *i == input => {
                // The pending native hold/export must lose publication authority
                // now; a later successful callback cannot turn Escape into a drag.
                self.dnd_abort("Escape during boundary preparation");
                if let State::Leaving { released, .. } = &mut self.dnd.state {
                    *released = true;
                }
                true
            }
            _ => false,
        }
    }

    /// `HeldSync` 里没有左键时是否仍保留为 `origin` 按着的左键（宿主还没得到输入源的确认）。
    pub(super) fn dnd_keep_left(&mut self, origin: DeviceId, _buttons: &[MouseButton]) -> bool {
        matches!(self.dnd.state, State::Hosting { input, .. } if input == origin)
    }

    /// 接受了 `from` 的 `FocusEnter { epoch }`，光标在 `pos`：配上之前收到的 `DndMsg::Enter` 就开始宿主。
    pub(super) fn dnd_focus_entered(&mut self, from: DeviceId, epoch: u64, pos: Point) {
        let Some(mut p) = self.dnd.pending.take() else {
            return;
        };
        if p.from != from || p.epoch != epoch {
            tracing::warn!(
                ?from,
                epoch,
                pending_from = ?p.from,
                pending_epoch = p.epoch,
                id = ?p.id,
                "DndMsg::Enter does not match this FocusEnter: the drag is dropped"
            );
            self.dnd.preview_requested = None;
            self.dnd.pending = Some(p);
            self.dnd_cancel_pending();
            return;
        }
        if !self.dnd_enabled() || self.holder != self.me() || self.epoch != epoch {
            self.dnd.preview_requested = None;
            self.dnd.pending = Some(p);
            self.dnd_cancel_pending();
            tracing::warn!(?from, "drag arrived but its focus or capability is no longer current");
            return;
        }
        p.focus_entered = true;
        if p.grant.is_none() {
            self.dnd.pending = Some(p);
            return;
        }
        let offer = Self::dnd_offer(p.id);
        let exported = self.dnd.sources.get(&offer).is_some_and(|source| source.exported);
        if p.id.origin == self.me() {
            let valid = (exported || self.dnd.native_source == Some(offer))
                && self.dnd.sources.get(&offer).is_some_and(|s| {
                    s.phase == source::Phase::Local
                        && s.holder
                            == (Holder {
                                device: self.me(),
                                epoch,
                            })
                        && s.input == p.input
                });
            if !valid {
                return;
            }
            if !exported {
                self.dnd_return_source_press(offer);
                if let Some(source) = self.dnd.sources.get_mut(&offer) {
                    source.native = NativeObservation::Resumed;
                }
                self.out.push(Output::Dnd(DndCmd::ResumeSource { id: p.id, epoch }));
                self.dnd.preview_requested = None;
                return;
            }
            // The exported copy can return to its own source. There is no
            // peer link to ourselves; use the source's retained artwork.
            p.image = self
                .dnd
                .image
                .clone()
                .filter(|(id, _)| *id == p.id)
                .map(|(_, image)| image);
            p.preview_ready = true;
        }
        if !p.preview_ready
            && self.dnd.preview_requested != p.preview_binding.map(|binding| (p.id.origin, p.id, epoch, binding))
            && p.id.origin != self.me()
        {
            // The predecessor owns only this focus handoff. Artwork belongs to
            // the original source, including after several stand-in hosts.
            if let Some(binding) = p.preview_binding {
                self.send(
                    p.id.origin,
                    Message::Dnd(DndMsg::PreviewRequest {
                        id: p.id,
                        epoch,
                        binding,
                    }),
                );
                self.dnd.preview_requested = Some((p.id.origin, p.id, epoch, binding));
            }
        }
        // Artwork is optional. Missing, slow or lost previews never hold Begin.
        if p.incomplete {
            p.focus_entered = true;
            self.dnd.pending = Some(p);
            return;
        }
        if exported {
            self.dnd_abort_host("the exported copy returned");
        } else {
            self.dnd_abort("a new drag arrived");
        }
        let me = self.me();
        let left = MouseButton::Left;
        if p.input == me && !self.buttons.contains_key(&left) {
            self.dnd_notify_source(p.id, SourceAction::Cancel);
            return;
        }
        self.hold_button_silently(p.input, left);
        tracing::info!(id = ?p.id, input = ?p.input, from = ?from, "hosting a drag");
        self.dnd.state = State::Hosting {
            id: p.id,
            input: p.input,
            items: p.items.clone(),
            push: None,
        };
        self.dnd.preview_requested = (p.id.origin != self.me())
            .then_some(p.preview_binding)
            .flatten()
            .map(|binding| (p.id.origin, p.id, epoch, binding));
        self.dnd.image = p.image.clone().map(|image| (p.id, image));
        // Always start with platform defaults; even an already received image
        // is applied only after the native start command has been submitted.
        self.out.push(Output::Dnd(DndCmd::SetImage(None)));
        let (key, revision, session) = p.grant.expect("paired grant");
        let preparation = self.dnd.preparations.get(&p.id.origin).cloned().unwrap_or_else(|| {
            DndPreparation::new(
                PreparationKey {
                    drag: key,
                    metadata_revision: 1,
                },
                session,
            )
        });
        let grant = DndTarget::new(preparation, Holder { device: me, epoch }, revision, p.input);
        self.dnd.target = Some(grant.clone());
        self.out.push(Output::Dnd(DndCmd::Begin {
            grant,
            id: p.id,
            epoch,
            at: pos,
            items: p.items,
            retained_inventory: p.retained_inventory,
        }));
        if let Some(image) = p.image {
            self.out
                .push(Output::Dnd(DndCmd::UpdateImage { id: p.id, epoch, image }));
        }
    }

    /// `from` 的 `FocusEnter { epoch }` 没有被接受（过时、暂停）：它带着的拖拽随之作废（只记录）。
    pub(super) fn dnd_focus_enter_ignored(&mut self, from: DeviceId, epoch: u64, why: &str) {
        if let Some(p) = self.dnd.pending.as_ref().filter(|p| p.from == from && p.epoch == epoch) {
            tracing::warn!(?from, epoch, id = ?p.id, why, "FocusEnter not accepted: the drag it carried is dropped");
            self.dnd_cancel_pending();
        }
    }

    /// Focus departure cancels probes and held hosts. A released native host
    /// retains its drop authority until the matching completion callback.
    pub(super) fn dnd_focus_left(&mut self) {
        // The source authority was moved before an intentional handoff. Its
        // native hold deliberately survives; only the local host leaves here.
        // On macOS 14, AppKit's successful Dock drop callback can follow the
        // physical UP and a pointer crossing back to the input device. Focus
        // departure must not cancel that released native session: QuickTime
        // opens its File Provider URL asynchronously after the callback. Keep
        // the original holder epoch until Ended; unstarted/held drags still
        // cancel normally. After release, user input cannot undo the drop.
        if self.dnd.target.as_ref().is_some_and(DndTarget::drop_pending) {
            return;
        }
        self.dnd_abort_host("focus left this device");
    }

    /// 对端发来 `Hello`（它开始了一条新连接，可能刚重启、换了版本）：先当作没有拖放能力，随后的 `Caps`
    /// （同一条有序流，紧跟在 `Hello` 之后）再设置。未确认能力前不能交出。
    pub(super) fn dnd_hello(&mut self, peer: DeviceId) {
        self.dnd.peers.remove(&peer);
        if self.dnd.preview_requested.is_some_and(|(from, _, _, _)| from == peer) {
            self.dnd.preview_requested = None;
        }
    }

    /// 对端断开或心跳超时（在释放它按下的键之前调用）。
    pub(super) fn dnd_peer_lost(&mut self, peer: DeviceId) {
        if let Some(preparation) = self.dnd.preparations.remove(&peer) {
            preparation.revoke();
            self.out.push(Output::Dnd(DndCmd::RevokePreparation { preparation }));
        }
        self.dnd.deliveries.retain(|key, grant| {
            if key.offer.clip.origin == peer {
                grant.revoke();
                false
            } else {
                true
            }
        });
        self.dnd.notices.remove(&peer);
        self.dnd.source_sessions.remove(&peer);
        self.dnd.input_notices.remove(&peer);
        self.dnd_cancel_exports(Some(peer));
        if let Some(offer) = self.dnd.native_source
            && self.dnd.sources.get(&offer).is_some_and(|s| {
                (s.input == peer || s.holder.device == peer)
                    && matches!(s.phase, source::Phase::Held | source::Phase::Released)
            })
        {
            self.dnd_abort_source();
        }
        // Heartbeat loss retains capabilities. A real connection/session loss
        // clears them in on_peer_disconnected; path replacement has no loss event.
        if self
            .dnd
            .pending
            .as_ref()
            .is_some_and(|p| p.from == peer || p.input == peer || p.id.origin == peer)
        {
            self.dnd_cancel_pending();
        }
        let abort = match &self.dnd.state {
            State::Probing { input, gesture, .. } => *input == peer || gesture.landing.screen.device == peer,
            State::Hosting { id, input, .. } => *input == peer || id.origin == peer,
            // `Released` 时检查目标与输入源是否还在。
            State::Leaving { .. } | State::Idle => false,
        };
        if abort {
            self.dnd_abort("peer lost");
        }
        // The native release and its receipt can straddle an input-stream
        // failure. Retain an outstanding receipt, never its claim on a future
        // click's UP. Unsubmitted native holds keep their release obligation.
        for press in self.dnd.presses.values_mut().filter(|p| p.input == peer) {
            press.retire_input_route();
        }
        self.dnd.presses.retain(|_, p| !p.retired());
    }

    pub(super) fn dnd_retire_physical_mouse(&mut self) {
        let me = self.me();
        let exports: Vec<_> = self
            .dnd
            .sources
            .values()
            .filter(|s| {
                s.input == me
                    && s.exported
                    && matches!(
                        s.phase,
                        source::Phase::Local | source::Phase::Held | source::Phase::Released
                    )
            })
            .map(|s| (s.offer, s.holder))
            .collect();
        for (offer, holder) in exports {
            self.dnd_source_action(holder.device, offer, holder, SourceAction::Cancel, None);
        }
        if self
            .dnd
            .native_source
            .and_then(|offer| self.dnd.sources.get(&offer))
            .is_some_and(|source| source.input == me)
        {
            self.dnd_abort_native_source();
        }
        self.dnd_escape(me);
    }

    /// 暂停、锁定、焦点被拿走、平台不可用：撤掉接收窗口 / 取消本机的拖拽。
    pub(super) fn dnd_abort(&mut self, why: &str) {
        self.dnd_abort_source();
        self.dnd_abort_host(why);
    }

    fn dnd_abort_source(&mut self) {
        self.dnd_cancel_exports(None);
        self.dnd_abort_native_source();
    }
    fn dnd_abort_native_source(&mut self) {
        let Some(offer) = self.dnd.native_source else {
            return;
        };
        let Some(source) = self.dnd.sources.get_mut(&offer) else {
            return;
        };
        if source.phase == source::Phase::Local && !matches!(self.dnd.state, State::Leaving { native: true, .. }) {
            source.advance();
            self.dnd_publish_state(
                offer,
                DragState::Revoked {
                    scope: RevokeScope::Drag,
                    reason: RevokeReason::Cancelled,
                },
            );
            self.dnd_return_source_press(offer);
            self.dnd_integrity_end(offer, IntegrityOutcome::Cancelled);
            self.dnd.sources.remove(&offer);
            self.dnd.native_source = None;
            self.out.push(Output::Dnd(DndCmd::ForgetSource { id: offer.clip }));
            self.out.push(Output::Dnd(DndCmd::ReleaseData { id: offer.clip }));
            return;
        }
        if source.cancel() {
            self.dnd_submit_source_finish(offer, false, self.dnd_cancel_geometry(offer, None));
            self.dnd_publish_state(
                offer,
                DragState::Revoked {
                    scope: RevokeScope::Drag,
                    reason: RevokeReason::Cancelled,
                },
            );
        }
        if let State::Leaving {
            native: true, released, ..
        } = &mut self.dnd.state
        {
            *released = true;
        }
    }

    fn dnd_cancel_exports(&mut self, peer: Option<DeviceId>) {
        let sources: Vec<_> = self
            .dnd
            .sources
            .values()
            .filter(|source| {
                source.exported
                    && matches!(
                        source.phase,
                        source::Phase::Local | source::Phase::Held | source::Phase::Released
                    )
                    && peer.is_none_or(|peer| source.input == peer || source.holder.device == peer)
            })
            .map(|source| (source.offer, source.holder))
            .collect();
        for (offer, holder) in sources {
            self.dnd_source_action(holder.device, offer, holder, SourceAction::Cancel, None);
        }
    }

    fn dnd_abort_host(&mut self, why: &str) {
        self.dnd_cancel_pending();
        match &self.dnd.state {
            State::Probing { .. } => self.dnd_unprobe(),
            State::Hosting { .. } => {
                tracing::info!(why, "cancelling the drag on this device");
                self.dnd_cancel_hosting();
            }
            State::Leaving { .. } | State::Idle => {}
        }
    }

    /// A second physical button press is a new local operation, never authority
    /// to drop a native hold retained for the previous drag.
    pub(super) fn dnd_local_press(&mut self) {
        if self.dnd_drop_released() {
            return;
        }
        if self.dnd.native_source.is_some() || matches!(self.dnd.state, State::Hosting { .. } | State::Leaving { .. }) {
            self.dnd_abort("local button interrupted drag");
        }
    }

    // ================= 内部 =================

    /// 除了输入源 `input` 的左键，没有别的按钮按着。
    fn left_only(&self, input: DeviceId) -> bool {
        let left = MouseButton::Left;
        let me = self.me();
        if matches!(self.dnd.state, State::Hosting { input: owner, .. } if owner == input) {
            return self.held.get(&input).is_some_and(|h| h.buttons.contains(&left))
                && !self.held.iter().any(|(o, h)| *o != input && !h.buttons.is_empty());
        }
        if input == me {
            let ok = matches!(
                self.buttons.get(&left),
                Some(Target::Local) | Some(Target::LocalInjected)
            ) && self.buttons.len() == 1;
            let others = self.held.iter().any(|(o, h)| *o != me && !h.buttons.is_empty());
            ok && !others
        } else {
            let ok = self
                .held
                .get(&input)
                .is_some_and(|h| h.buttons.len() == 1 && h.buttons.contains(&left));
            let others = self.held.iter().any(|(o, h)| *o != input && !h.buttons.is_empty());
            ok && !others && self.buttons.is_empty()
        }
    }

    fn dnd_unprobe(&mut self) {
        if matches!(self.dnd.state, State::Probing { .. }) {
            // A resumed native drag may start and abandon another probe before
            // being detected again. Retire its source through the same path as
            // recapture, including the returned press observation; an orphan
            // would compete with the next capture for the one original UP.
            self.dnd_abort_source();
            self.dnd.state = State::Idle;
            self.out.push(Output::Dnd(DndCmd::Unprobe));
        }
    }

    fn dnd_detected(&mut self, id: ClipId, new_items: Vec<ClipData>) {
        let State::Probing { input, .. } = self.dnd.state else {
            tracing::debug!(?id, "drag detected but no longer probing: ignored");
            return;
        };
        self.dnd_source_learned(id, input, new_items.clone());
        let State::Probing { content, gesture, .. } = &mut self.dnd.state else {
            return;
        };
        *content = Some((id, new_items));
        if gesture.crossed(self.cfg.settings.dnd_push_fraction) {
            self.dnd_leave(true);
        }
    }

    /// Metadata discovery is independent of edge crossing. Earlier native
    /// detection can call this same entry point without transferring focus.
    pub fn dnd_source_learned(&mut self, id: ClipId, input: DeviceId, items: Vec<ClipData>) {
        if id.origin != self.me() || !self.dnd_enabled() || (input == self.me() && self.mouse_local()) {
            return;
        }
        let offer = Self::dnd_offer(id);
        if self.dnd.native_source.is_some_and(|old| old != offer) {
            self.dnd_abort_source();
        }
        let holder = Holder {
            device: self.me(),
            epoch: self.epoch,
        };
        let source = self
            .dnd
            .sources
            .entry(offer)
            .or_insert_with(|| source::Source::new(offer, input, holder));
        source.metadata(items);
        self.dnd.native_source = Some(offer);
        self.dnd_announce_members(offer);
    }

    /// 准备离开：让平台结束本机的拖拽（`native`：源程序的拖拽 → `Release`；否则 `Cancel`）。
    fn dnd_leave(&mut self, native: bool) {
        let state = std::mem::take(&mut self.dnd.state);
        let (input, gesture, id, items) = match state {
            State::Probing {
                input,
                gesture,
                content: Some((id, items)),
                ..
            } => (input, gesture, id, items),
            State::Hosting {
                id,
                input,
                items,
                push: Some(gesture),
                ..
            } => (input, gesture, id, items),
            other => {
                self.dnd.state = other;
                return;
            }
        };
        let dst = &gesture.landing;
        let crossing = gesture.crossing.unwrap_or_else(std::time::Instant::now);
        tracing::info!(?id, ?input, to = ?dst.screen.device, native, "handing the drag over: ending it here first");
        // Native release requires Local; hosted return uses LocalInjected while the physical hold is still ours.
        let cancel_physical = !native && input == self.me() && self.buttons.contains_key(&MouseButton::Left);
        // 为远端源（或本机由 Oink 注入）按着的左键由平台抬起：路由这里只记账，不注入。
        if !native {
            if let Some(grant) = self.dnd.target.take() {
                grant.leave();
            }
            self.dnd_retire_preparation(id.origin);
            self.release_button_silently(input, MouseButton::Left);
        }
        let target_os = self
            .peers
            .get(&dst.screen.device)
            .map_or(crate::config::Os::Other, |peer| peer.os);
        self.dnd.state = State::Leaving {
            epoch: self.epoch,
            input,
            gesture,
            id,
            items,
            native,
            released: false,
        };
        if native {
            let offer = Self::dnd_offer(id);
            self.dnd.sources.entry(offer).or_insert_with(|| {
                source::Source::new(
                    offer,
                    input,
                    Holder {
                        device: id.origin,
                        epoch: self.epoch,
                    },
                )
            });
            if let Some(source) = self.dnd.sources.get_mut(&offer) {
                if source.origin.is_none() {
                    source.origin = self.dnd.press_origin.clone();
                }
                source.exit = self.pointer.as_ref().map(|p| p.local);
            }
            self.dnd.native_source = Some(offer);
            self.dnd_take_source_press(offer, input);
            if let Some(source) = self.dnd.sources.get_mut(&offer) {
                source.ever_held = true;
                source.native = NativeObservation::HoldRequested;
            }
            self.out.push(Output::Dnd(DndCmd::HoldSource {
                origin: self
                    .dnd
                    .sources
                    .get(&offer)
                    .and_then(|source| source.origin.as_ref().map(|p| p.local)),
                target: target_os,
                crossing,
                id,
                epoch: self.epoch,
                physical: input == self.me(),
            }));
            return;
        }
        self.out.push(Output::Dnd(DndCmd::LeaveHost {
            id,
            epoch: self.epoch,
            physical: cancel_physical,
        }));
    }

    fn dnd_released(&mut self, ok: bool) {
        if let State::Leaving { input, .. } = &self.dnd.state
            && let Some(current) = self.pointer.clone()
        {
            self.dnd_follow_edge(*input, &current, (0., 0.));
        }
        let State::Leaving {
            epoch,
            input,
            gesture,
            id,
            items,
            native,
            released,
        } = std::mem::take(&mut self.dnd.state)
        else {
            return;
        };
        let dst = gesture.landing;
        let me = self.me();
        let to = dst.screen.device;
        let why = if !ok {
            Some("the local drag did not end on the catcher")
        } else if self.fully_paused {
            Some("sharing was paused during native hold")
        } else if released {
            Some("the left button was released meanwhile")
        } else if self.holder != me || self.epoch != epoch {
            Some("focus already left this device")
        } else if self.system_clip_active() {
            Some("system cursor clip prevents crossing")
        } else if self.layout.reference_per_native_unit(&dst.screen).is_none()
            || !self.input_ready(to)
            || !self.dnd.peers.contains(&to)
        {
            Some("the target device is no longer available")
        } else if input != me && !self.reachable(input) {
            Some("the device holding the button is gone")
        } else if input == me && !self.buttons.contains_key(&MouseButton::Left) {
            Some("the left button is no longer held")
        } else {
            None
        };
        if let Some(why) = why {
            tracing::warn!(?id, why, native, "drag not handed over");
            // URL preparation may still be pending when the pointer leaves.
            // With no native host to cancel, HostLeft(false) must still release
            // the original held source, just like Hosting's failed Ended.
            if !native {
                self.dnd_notify_source(id, SourceAction::Cancel);
            }
            if native && ok {
                let offer = Self::dnd_offer(id);
                if self.dnd.sources.get(&offer).is_some_and(|s| s.exported) {
                    self.dnd_integrity_end(offer, IntegrityOutcome::Cancelled);
                    self.dnd.sources.remove(&offer);
                    self.out.push(Output::Dnd(DndCmd::ReleaseData { id }));
                    return;
                }
                if let Some(source) = self.dnd.sources.get_mut(&offer) {
                    source.cancel();
                }
                self.dnd_submit_source_finish(offer, false, None);
                self.dnd_publish_state(
                    offer,
                    DragState::Revoked {
                        scope: RevokeScope::Drag,
                        reason: RevokeReason::Cancelled,
                    },
                );
            } else if native {
                let offer = Self::dnd_offer(id);
                self.dnd_return_source_press(offer);
                if let Some(source) = self.dnd.sources.get_mut(&offer) {
                    source.advance();
                    source.native = NativeObservation::Unconfirmed(DndSourceEffect::Hold);
                    source.phase = source::Phase::Closed;
                }
                self.dnd_publish_state(
                    offer,
                    DragState::Revoked {
                        scope: RevokeScope::Drag,
                        reason: RevokeReason::HoldFailed,
                    },
                );
                self.dnd_integrity_end(offer, IntegrityOutcome::HoldRejected);
                self.dnd.sources.remove(&offer);
                self.out.push(Output::Dnd(DndCmd::ReleaseData { id }));
                if self.dnd.native_source == Some(offer) {
                    self.dnd.native_source = None;
                }
            }
            return;
        }
        if native && let Some(source) = self.dnd.sources.get_mut(&Self::dnd_offer(id)) {
            source.confirmed = true;
            source.native = if source.exported {
                NativeObservation::Exported
            } else {
                NativeObservation::Held
            };
            source.metadata(items);
        }
        self.dnd_notify_source(
            id,
            SourceAction::Handoff {
                next: Holder {
                    device: to,
                    epoch: self.epoch + 1,
                },
            },
        );
        self.dnd.outgoing = Some(Outgoing { to, id });
        if let Some((from, delta, zone)) = self.dnd.crossing_evidence.take()
            && let Some(d) = self.diagnostics.as_ref().filter(|d| d.enabled())
        {
            d.crossing_with_zone(
                self.layout.clone(),
                self.epoch + 1,
                &from,
                Point::new(delta.0, delta.1),
                true,
                false,
                zone,
                &dst,
                self.departure_evidence(&from),
            );
        }
        self.hand_off(dst, self.trail_pointer());
    }

    /// `hand_off` 在发 `FocusEnter` 之前取走要一起发的 `DndMsg::Enter`。
    pub(super) fn take_dnd_enter(&mut self, to: DeviceId, epoch: u64) -> Option<Message> {
        let o = self.dnd.outgoing.take()?;
        if o.to != to {
            return None;
        }
        Some(Message::Dnd(DndMsg::Enter { id: o.id, epoch }))
    }

    fn dnd_cancel_hosting(&mut self) {
        let State::Hosting { id, input, .. } = std::mem::take(&mut self.dnd.state) else {
            return;
        };
        let holder = self.dnd.target.take().map_or(
            Holder {
                device: self.me(),
                epoch: self.epoch,
            },
            |grant| {
                grant.revoke();
                grant.holder
            },
        );
        let physical = input == self.me() && self.buttons.contains_key(&MouseButton::Left);
        self.release_button_silently(input, MouseButton::Left);
        self.dnd_retire_preparation(id.origin);
        self.out.push(Output::Dnd(DndCmd::Cancel {
            id,
            epoch: holder.epoch,
            physical,
        }));
        self.dnd_notify_source_at(id, holder, SourceAction::Cancel);
    }

    /// 记下为 `origin` 按着 `button`（平台负责实际的按下）。
    fn hold_button_silently(&mut self, origin: DeviceId, button: MouseButton) {
        let h = self.held.entry(origin).or_default();
        if h.buttons.insert(button) {
            self.button_diagnostics.press(origin, button, self.dnd.now, None);
            *self.button_refs.entry(button).or_insert(0) += 1;
        }
    }

    /// 不再为 `origin` 按着 `button`（平台负责实际的抬起）。
    fn release_button_silently(&mut self, origin: DeviceId, button: MouseButton) {
        let Some(h) = self.held.get_mut(&origin) else {
            return;
        };
        if !h.buttons.remove(&button) {
            return;
        }
        self.button_diagnostics.release(origin, button);
        if let Some(rc) = self.button_refs.get_mut(&button) {
            *rc -= 1;
            if *rc == 0 {
                self.button_refs.remove(&button);
            }
        }
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    use crate::dnd::DND_SEQ_BIT;
    use crate::{Os, RouterConfig};
    use std::sync::Arc;
    fn preview_binding(id: ClipId) -> PreviewBinding {
        PreviewBinding {
            key: DragKey {
                offer: OfferId {
                    clip: id,
                    generation: id.seq,
                },
                native_generation: id.seq,
            },
            metadata_revision: 1,
            preview_revision: 1,
        }
    }
    fn offered(r: &Router, id: ClipId, epoch: u64, input: DeviceId, items: Vec<ClipData>) -> DragNotice {
        DragNotice {
            key: DragKey {
                offer: Router::dnd_offer(id),
                native_generation: id.seq,
            },
            revision: epoch,
            input,
            state: DragState::Offered {
                holder: Holder { device: r.me(), epoch },
                mode: SourceMode::Held,
            },
            metadata: MetadataVersion {
                revision: 1,
                readiness: MetadataReadiness::Ready,
            },
            preview: PreviewAvailability::Available { revision: 1 },
            retained_inventory: None,
            items,
        }
    }
    fn release_notice(r: &mut Router, from: DeviceId, id: ClipId, epoch: u64, now: u64) {
        let mut n = offered(r, id, epoch, from, Vec::new());
        n.revision += 1;
        n.state = DragState::Released {
            holder: Holder { device: r.me(), epoch },
            mode: SourceMode::Held,
        };
        r.on_dnd_notice(from, n, false, now);
    }
    fn pending(r: &mut Router, from: DeviceId, id: ClipId, epoch: u64, now: u64) {
        r.on_dnd_msg(from, DndMsg::Enter { id, epoch });
        r.on_dnd_notice(from, offered(r, id, epoch, from, Vec::new()), true, now);
    }

    #[test]
    fn mac_cancel_maps_holder_position_and_falls_back_to_exit_with_epoch_guard() {
        use crate::layout::ScreenId;
        use oink_foundation::{Rect, display::DisplayInfo};
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let bounds = Rect::new(0., 0., 100., 100.);
        let mut layout = crate::SimpleLayout::new();
        // Source coordinates have twice the units of the effective plane plane.
        layout.add_screen(ScreenId::new(me, "screen"), bounds, Rect::new(20., 30., 50., 50.));
        layout.add_screen(ScreenId::new(peer, "screen"), bounds, Rect::new(80., 30., 50., 50.));
        let display = DisplayInfo {
            key: "screen".into(),
            name: "screen".into(),
            bounds,
            pixel_width: 200,
            pixel_height: 200,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        };
        let mut production = oink_display::layout::Layout::default();
        for (device, rect) in [
            (me, Rect::new(20., 30., 50., 50.)),
            (peer, Rect::new(80., 30., 50., 50.)),
        ] {
            let mut screen =
                oink_display::layout::ScreenPlacement::from_display(device, &display, Point::new(rect.x, rect.y));
            screen.rect = rect;
            production.screens.push(screen);
        }
        let layouts: [Arc<dyn crate::layout::LayoutQuery>; 2] = [
            Arc::new(layout),
            Arc::new(crate::display_layout::TopologyLayout::from_layout(
                &production,
                &Default::default(),
            )),
        ];
        for layout in layouts {
            let screen = ScreenId::new(me, "screen");
            for plane in [Point::new(95., 65.), Point::new(55., 0.)] {
                assert!(bounds.contains(layout.from_plane(&screen, plane).unwrap()));
                assert!(!bounds.contains(layout.from_plane_unclamped(&screen, plane).unwrap()));
            }
            assert!(
                layout
                    .from_plane_unclamped(&ScreenId::new(me, "missing"), Point::new(0., 0.))
                    .is_none()
            );
            for os in [Os::MacOS, Os::Windows] {
                for (remote, expected) in [
                    (Some(Point::new(95., 65.)), Point::new(100., 60.)),
                    // Final holder moved above the source after leaving at right.
                    (Some(Point::new(55., 0.)), Point::new(50. + 100. / 11., 0.)),
                    (None, Point::new(99., 20.)),
                ] {
                    let mut r = Router::new(
                        RouterConfig::new(
                            oink_foundation::session::Boot {
                                device_id: me,
                                nonce: [me.0[0]; 16],
                            },
                            "local",
                            os,
                        ),
                        layout.clone(),
                    );
                    r.set_local_displays(vec![DisplayInfo {
                        key: "screen".into(),
                        name: "screen".into(),
                        bounds,
                        pixel_width: 200,
                        pixel_height: 200,
                        reference_extent: None,
                        rotation_deg: 0,
                        is_primary: true,
                        is_builtin: false,
                        is_virtual: false,
                    }]);
                    let offer = Router::dnd_offer(ClipId {
                        origin: me,
                        seq: DND_SEQ_BIT | 1,
                    });
                    let local = Holder { device: me, epoch: 1 };
                    let holder = Holder { device: peer, epoch: 2 };
                    let mut source = source::Source::new(offer, me, local);
                    source.origin = Some(ScreenPos::new(ScreenId::new(me, "screen"), Point::new(50., 50.)));
                    source.exit = Some(Point::new(99., 20.));
                    source.handoff(local, holder);
                    source.ever_held = true;
                    r.dnd.sources.insert(offer, source);
                    r.dnd_take_source_press(offer, r.dnd.sources[&offer].input);
                    r.dnd.native_source = Some(offer);
                    r.holder = peer;
                    r.out.clear();
                    r.on_dnd_event(DndEvent::SourceCancelRetired { id: offer.clip });
                    assert_eq!(r.dnd.sources[&offer].phase, source::Phase::Held);
                    r.dnd_source_action(peer, offer, local, SourceAction::Cancel, remote);
                    assert!(r.out.is_empty(), "stale holder cannot position or cancel the source");
                    r.dnd_source_action(peer, offer, holder, SourceAction::Cancel, remote);
                    let (at, geometry) = r
                        .out
                        .iter()
                        .find_map(|o| match o {
                            Output::Dnd(DndCmd::FinishSource {
                                accept: false,
                                cancel_at,
                                cancel_geometry,
                                ..
                            }) => Some((*cancel_at, *cancel_geometry)),
                            _ => None,
                        })
                        .expect("cancel closes the native source");
                    if os == Os::Windows {
                        assert!(at.is_none());
                        assert!(geometry.is_none());
                    } else {
                        let at = at.unwrap();
                        let geometry = geometry.unwrap();
                        assert_eq!(geometry.down, Some(Point::new(50., 50.)));
                        assert_eq!(geometry.target_plane, remote);
                        assert_eq!(geometry.requested(), Some(at));
                        assert_eq!(geometry.fallback.is_some(), remote.is_none());
                        assert!(
                            (at.x - expected.x).abs() < 0.001 && (at.y - expected.y).abs() < 0.001,
                            "{at:?}"
                        );
                        assert!(bounds.contains(at));
                        // Receipt may arrive before or after the deferred exit;
                        // neither order may label it native ended or catcher loss.
                        r.out.clear();
                        let receipt_first = remote.is_some();
                        if receipt_first {
                            r.on_dnd_event(DndEvent::SourceButtonReleased {
                                id: offer.clip,
                                ok: true,
                            });
                        }
                        r.on_dnd_event(DndEvent::SourceCancelRetired { id: offer.clip });
                        if !receipt_first {
                            r.on_dnd_event(DndEvent::SourceButtonReleased {
                                id: offer.clip,
                                ok: true,
                            });
                        }
                        let record = r
                            .out
                            .iter()
                            .find_map(|output| match output {
                                Output::Dnd(DndCmd::Integrity { record, .. }) => Some(record),
                                _ => None,
                            })
                            .unwrap();
                        assert_eq!(record.outcome, IntegrityOutcome::Cancelled);
                        assert_eq!(record.native, NativeObservation::CancelRetired);
                    }
                }
            }
        }
    }

    #[test]
    fn hosted_drag_tangent_motion_keeps_push_but_inward_motion_clears_it() {
        use crate::layout::ScreenId;
        use oink_foundation::{Rect, display::DisplayInfo};
        for remote in [false, true] {
            let me = DeviceId([1; 32]);
            let peer = DeviceId([2; 32]);
            let bounds = Rect::new(0., 0., 1000., 600.);
            let mut layout = crate::SimpleLayout::new();
            layout.add_screen(
                ScreenId::new(me, "screen"),
                bounds,
                Rect::new(bounds.x / 686., bounds.y / 686., bounds.w / 686., bounds.h / 686.),
            );
            layout.add_screen(
                ScreenId::new(peer, "screen"),
                bounds,
                Rect::new(0. / 686.0, 620. / 686.0, 1000. / 686.0, 600. / 686.0),
            );
            let mut r = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: me,
                        nonce: [me.0[0]; 16],
                    },
                    "local",
                    Os::Windows,
                ),
                Arc::new(layout),
            );
            r.set_local_displays(vec![DisplayInfo {
                key: "screen".into(),
                name: "screen".into(),
                bounds,
                pixel_width: 1000,
                pixel_height: 600,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: true,
                is_builtin: false,
                is_virtual: false,
            }]);
            r.set_dnd_available(true);
            r.test_message(peer, Message::Dnd(DndMsg::Caps { enabled: true }), 1);
            r.peers.get_mut(&peer).unwrap().focus_ready = true;
            let input = if remote { peer } else { me };
            if remote {
                r.held.entry(peer).or_default().buttons.insert(MouseButton::Left);
                r.button_refs.insert(MouseButton::Left, 1);
                r.pointer = Some(ScreenPos::new(
                    ScreenId::new(me, "screen"),
                    Point::new(400., 599.999999),
                ));
            } else {
                r.buttons.insert(MouseButton::Left, Target::Local);
            }
            let motion = |r: &mut Router, dx, dy, cursor, now| {
                if remote {
                    r.remote_motion(peer, dx / 686., dy / 686., now);
                } else {
                    r.local_motion(dx, dy, Some(cursor), now);
                }
            };
            let hosted = || State::Hosting {
                id: ClipId {
                    origin: peer,
                    seq: DND_SEQ_BIT | 1,
                },
                input,
                items: vec![ClipData::Text("drag".into())],
                push: None,
            };
            r.dnd.state = hosted();
            r.hold_button_silently(input, MouseButton::Left);
            if !remote {
                r.buttons.insert(MouseButton::Left, Target::LocalInjected);
            }
            motion(&mut r, 0., 10., Point::new(400., 600.), 2);
            assert!(matches!(r.dnd.state, State::Hosting { push: Some(_), .. }));
            r.out.clear();
            motion(&mut r, 20., 0., Point::new(420., 599.), 3);
            motion(&mut r, 0., 0., Point::new(420., 599.), 4);
            assert!(
                matches!(r.dnd.state, State::Hosting { push: Some(_), .. }),
                "remote={remote}: a tangent/zero move discarded hosted push"
            );
            assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Probe { .. }))));
            motion(&mut r, 0., 20., Point::new(420., 600.), 5);
            assert!(
                matches!(r.dnd.state, State::Leaving { native: false, .. }),
                "remote={remote}: hosted return needs only the original total depth"
            );

            r.dnd.state = hosted();
            r.hold_button_silently(input, MouseButton::Left);
            if !remote {
                r.buttons.insert(MouseButton::Left, Target::LocalInjected);
            }
            motion(&mut r, 0., 10., Point::new(420., 600.), 6);
            assert!(matches!(r.dnd.state, State::Hosting { push: Some(_), .. }));
            motion(&mut r, 0., -1., Point::new(420., 599.), 7);
            assert!(matches!(r.dnd.state, State::Hosting { push: None, .. }));
        }
    }

    fn native_drag_tangent_cases(cases: &[(bool, f64, f64, f64)]) {
        use crate::layout::ScreenId;
        use oink_foundation::{Rect, display::DisplayInfo};
        for &(remote, start_x, tangent_dx, end_x) in cases {
            let me = DeviceId([1; 32]);
            let peer = DeviceId([2; 32]);
            let bounds = Rect::new(0., 0., 1000., 600.);
            let mut layout = crate::SimpleLayout::new();
            layout.add_screen(
                ScreenId::new(me, "screen"),
                bounds,
                Rect::new(bounds.x / 686., bounds.y / 686., bounds.w / 686., bounds.h / 686.),
            );
            layout.add_screen(
                ScreenId::new(peer, "screen"),
                bounds,
                Rect::new(0. / 686.0, 620. / 686.0, 1000. / 686.0, 600. / 686.0),
            );
            let mut r = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: me,
                        nonce: [me.0[0]; 16],
                    },
                    "local",
                    Os::Windows,
                ),
                Arc::new(layout),
            );
            r.set_local_displays(vec![DisplayInfo {
                key: "screen".into(),
                name: "screen".into(),
                bounds,
                pixel_width: 1000,
                pixel_height: 600,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: true,
                is_builtin: false,
                is_virtual: false,
            }]);
            r.set_dnd_available(true);
            r.test_message(peer, Message::Dnd(DndMsg::Caps { enabled: true }), 1);
            r.peers.get_mut(&peer).unwrap().focus_ready = true;
            let input = if remote { peer } else { me };
            if remote {
                r.held.entry(peer).or_default().buttons.insert(MouseButton::Left);
                r.button_refs.insert(MouseButton::Left, 1);
                r.pointer = Some(ScreenPos::new(
                    ScreenId::new(me, "screen"),
                    Point::new(start_x, 599.999999),
                ));
            } else {
                r.buttons.insert(MouseButton::Left, Target::Local);
            }
            let motion = |r: &mut Router, dx, dy, cursor, now| {
                if remote {
                    r.remote_motion(peer, dx / 686., dy / 686., now);
                } else {
                    r.local_motion(dx, dy, Some(cursor), now);
                }
            };
            motion(&mut r, 0., 10., Point::new(start_x, 600.), 2);
            let id = ClipId {
                origin: me,
                seq: DND_SEQ_BIT | 1,
            };
            r.on_dnd_event(DndEvent::Detected {
                id,
                items: vec![ClipData::Text("drag".into())],
            });
            assert_eq!(r.dnd_state_name(), "ready");
            r.out.clear();
            // Windows clamps the native cursor to the last integer pixel. A tangent
            // report on that pixel is still on the same edge, not an inward retreat.
            motion(&mut r, tangent_dx, 0., Point::new(start_x + tangent_dx, 599.), 3);
            assert_eq!(r.dnd_state_name(), "ready");
            assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Unprobe))));
            if end_x != start_x {
                assert!(r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Probe { .. }))));
            }
            motion(&mut r, 0., 20., Point::new(end_x, 600.), 4);
            assert_eq!(r.dnd_state_name(), "leaving", "tangent discarded earlier push/content");
            // An inward report clears even while the cursor remains in the edge band.
            r.dnd.state = State::Probing {
                input,
                gesture: EdgeGesture::fixture(r.me(), peer, Point::new(end_x, 599.999999), (0., 1.), 0.01),
                content: Some((id, vec![ClipData::Text("drag".into())])),
            };
            r.out.clear();
            motion(&mut r, 0., -1., Point::new(end_x, 599.), 5);
            assert_eq!(r.dnd_state_name(), "idle");
            assert!(r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Unprobe))));
        }
    }

    #[test]
    fn native_drag_tangent_motion_keeps_content_and_push_but_inward_motion_clears_them() {
        native_drag_tangent_cases(&[(false, 400., 20., 420.), (true, 400., 20., 420.)]);
    }

    #[test]
    fn native_drag_corner_tangent_keeps_content_until_outward_push() {
        native_drag_tangent_cases(&[(false, 0., -1., 0.)]);
    }

    #[test]
    fn upper_display_drag_push_bypasses_dock_damping_for_native_and_hosted_sources() {
        use crate::ScreenId;
        use oink_foundation::{
            Rect,
            display::DisplayInfo,
            input::{EdgeResistanceRegion, ScreenEdge},
        };
        for remote in [false, true] {
            for native in [false, true] {
                let me = DeviceId([1; 32]);
                let peer = DeviceId([2; 32]);
                let primary = Rect::new(0., 0., 1728., 1117.);
                let upper = Rect::new(1560., -1080., 1920., 1080.);
                let target = Rect::new(0., 0., 1920., 1080.);
                let mut layout = crate::SimpleLayout::new();
                for (device, key, bounds, plane) in [
                    (me, "primary", primary, primary),
                    (me, "upper", upper, upper),
                    (peer, "screen", target, Rect::new(1728., 0., 1920., 1080.)),
                ] {
                    layout.add_screen(
                        ScreenId::new(device, key),
                        bounds,
                        Rect::new(plane.x / 686., plane.y / 686., plane.w / 686., plane.h / 686.),
                    );
                }
                let mut cfg = RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: me,
                        nonce: [1; 16],
                    },
                    "local",
                    Os::MacOS,
                );
                cfg.settings.damping.platform_profile = crate::damping::DampingProfile::DepthOnly { depth: 1000. };
                cfg.settings.dnd_push_fraction = 0.005;
                let mut r = Router::new(cfg, Arc::new(layout));
                r.set_local_displays(
                    [("primary", primary), ("upper", upper)]
                        .into_iter()
                        .map(|(key, bounds)| DisplayInfo {
                            key: key.into(),
                            name: key.into(),
                            bounds,
                            pixel_width: bounds.w as u32,
                            pixel_height: bounds.h as u32,
                            reference_extent: None,
                            rotation_deg: 0,
                            is_primary: key == "primary",
                            is_builtin: key == "primary",
                            is_virtual: false,
                        })
                        .collect(),
                );
                // Bottom Dock on both displays models Separate Spaces. An
                // enormous ordinary damping threshold must not delay DnD.
                r.set_edge_resistance_regions(
                    ["primary", "upper"]
                        .into_iter()
                        .map(|display| EdgeResistanceRegion {
                            display: display.into(),
                            edges: vec![ScreenEdge::Bottom],
                            corners: vec![],
                        })
                        .collect(),
                );
                r.set_dnd_available(true);
                r.test_message(peer, Message::Dnd(DndMsg::Caps { enabled: true }), 1);
                r.peers.get_mut(&peer).unwrap().focus_ready = true;
                let input = if remote { peer } else { me };
                let id = ClipId {
                    origin: if native { me } else { peer },
                    seq: DND_SEQ_BIT | 1,
                };
                if remote {
                    r.held.entry(peer).or_default().buttons.insert(MouseButton::Left);
                    r.button_refs.insert(MouseButton::Left, 1);
                } else {
                    r.buttons.insert(
                        MouseButton::Left,
                        if native { Target::Local } else { Target::LocalInjected },
                    );
                }
                let at = Point::new(2577.319, -0.000005019685);
                r.pointer = Some(ScreenPos::new(ScreenId::new(me, "upper"), at));
                if !native {
                    r.hold_button_silently(input, MouseButton::Left);
                    r.dnd.state = State::Hosting {
                        id,
                        input,
                        items: vec![ClipData::Text("drag".into())],
                        push: None,
                    };
                }
                let motion = |r: &mut Router, dy, now| {
                    if remote {
                        r.remote_motion(peer, 0., dy / 686., now);
                    } else {
                        r.local_motion(0., dy, Some(at), now);
                    }
                };
                motion(&mut r, 1., 2);
                assert!(r.resistance.is_none(), "ordinary damping must not own the drag");
                if native {
                    assert!(
                        r.out
                            .iter()
                            .any(|o| matches!(o, Output::Dnd(DndCmd::Probe { at }) if at.y < 0.))
                    );
                    r.on_dnd_event(DndEvent::Detected {
                        id,
                        items: vec![ClipData::Text("drag".into())],
                    });
                }
                motion(&mut r, 20., 3);
                assert!(
                    matches!(r.dnd.state, State::Leaving { native: got, .. } if got == native),
                    "remote={remote}, native={native}: DnD push must cross despite Dock damping"
                );
                assert!(r.resistance.is_none());
            }
        }
    }

    #[test]
    fn pending_drag_survives_duplicate_active_but_is_cleared_on_session_revocation() {
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: me,
                    nonce: [me.0[0]; 16],
                },
                "local",
                Os::Other,
            ),
            crate::router::connection_tests::session_layout(me, peer),
        );
        r.set_dnd_available(true);
        r.test_message(peer, Message::Dnd(DndMsg::Caps { enabled: true }), 1);
        r.test_message(
            peer,
            Message::Dnd(DndMsg::Enter {
                id: ClipId {
                    origin: peer,
                    seq: DND_SEQ_BIT | 1,
                },
                epoch: 2,
            }),
            2,
        );
        assert!(r.dnd.pending.is_some());
        r.test_connect(peer, 3);
        assert!(r.dnd.peers.contains(&peer));
        assert!(r.dnd.pending.is_some());
        r.test_disconnect(peer, 4);
        assert!(!r.dnd.peers.contains(&peer));
        assert!(r.dnd.pending.is_none());
        // The replacement's own SessionInit supplies its fresh capabilities.
        r.test_connect(peer, 5);
        assert!(r.dnd.peers.contains(&peer));
        assert!(r.dnd.pending.is_none());
    }
    #[test]
    fn deferred_items_pair_with_focus_in_either_order_and_use_current_pointer() {
        for items_first in [false, true] {
            let me = DeviceId([1; 32]);
            let peer = DeviceId([2; 32]);
            let id = ClipId {
                origin: peer,
                seq: DND_SEQ_BIT | 1,
            };
            let mut r = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: me,
                        nonce: [me.0[0]; 16],
                    },
                    "local",
                    Os::Other,
                ),
                crate::router::connection_tests::session_layout(me, peer),
            );
            r.set_dnd_available(true);
            r.test_connect(peer, 1);
            pending(&mut r, peer, id, 2, 2);
            r.out.clear();
            let items = vec![ClipData::Text("drag".into())];
            if items_first {
                r.on_dnd_notice(peer, offered(&r, id, 2, peer, items.clone()), false, 3);
            }
            assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
            r.test_message(
                peer,
                Message::Control(crate::control::ControlMsg::FocusEnter {
                    epoch: 2,
                    display: "screen".into(),
                    pos: Point::new(2., 3.),
                    scope: Some(r.new_focus_scope()),
                    pointer_source: Some(peer),
                }),
                4,
            );
            if !items_first {
                assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
                assert!(
                    !r.held
                        .get(&peer)
                        .is_some_and(|h| h.buttons.contains(&MouseButton::Left))
                );
                r.pointer.as_mut().unwrap().local = Point::new(8., 9.);
                r.on_dnd_notice(peer, offered(&r, id, 2, peer, items), false, 5);
            }
            r.test_message(
                peer,
                Message::Dnd(DndMsg::Preview {
                    binding: preview_binding(id),
                    id,
                    epoch: 2,
                    image: None,
                }),
                6,
            );
            let expected = if items_first {
                Point::new(2., 3.)
            } else {
                Point::new(8., 9.)
            };
            assert!(
                r.out
                    .iter()
                    .any(|o| matches!(o, Output::Dnd(DndCmd::Begin { at, .. }) if *at == expected))
            );
        }
    }

    #[test]
    fn release_cancel_expiry_and_session_loss_cannot_revive_pending_drag() {
        for cancellation in 0..5 {
            let me = DeviceId([1; 32]);
            let peer = DeviceId([2; 32]);
            let id = ClipId {
                origin: peer,
                seq: DND_SEQ_BIT | 1,
            };
            let mut r = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: me,
                        nonce: [me.0[0]; 16],
                    },
                    "local",
                    Os::Other,
                ),
                crate::router::connection_tests::session_layout(me, peer),
            );
            r.set_dnd_available(true);
            r.test_connect(peer, 1);
            pending(&mut r, peer, id, 2, 2);
            r.test_message(
                peer,
                Message::Control(crate::control::ControlMsg::FocusEnter {
                    epoch: 2,
                    display: "screen".into(),
                    pos: Point::new(2., 3.),
                    scope: Some(r.new_focus_scope()),
                    pointer_source: Some(peer),
                }),
                3,
            );
            match cancellation {
                0 => release_notice(&mut r, peer, id, 2, 4),
                1 => {
                    r.dnd_escape(peer);
                }
                2 => r.dnd_focus_left(),
                3 => r.test_disconnect(peer, 4),
                _ => {
                    r.on_dnd_msg(
                        peer,
                        DndMsg::Refused {
                            id,
                            holder: Holder { device: me, epoch: 2 },
                            reason: RevokeReason::PreparationFailed,
                        },
                    );
                }
            }
            r.out.clear();
            r.on_dnd_notice(
                peer,
                offered(&r, id, 2, peer, vec![ClipData::Text("late".into())]),
                false,
                10_000_004,
            );
            assert!(
                !r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))),
                "cancellation {cancellation}"
            );
            assert!(
                !r.held
                    .get(&peer)
                    .is_some_and(|h| h.buttons.contains(&MouseButton::Left))
            );
        }
    }
    fn preview_image(width: f64) -> DragImage {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1_u32.to_be_bytes());
        png.extend_from_slice(&1_u32.to_be_bytes());
        png.resize(33, 0);
        let image = DragImage {
            png: png.into(),
            width,
            height: 1.,
            hotspot_x: 0.,
            hotspot_y: 0.,
        };
        assert!(image.valid());
        image
    }

    fn preview_wait() -> (Router, DeviceId, ClipId) {
        target_setup(false)
    }
    fn metadata_wait() -> (Router, DeviceId, ClipId) {
        target_setup(true)
    }
    fn target_setup(incomplete: bool) -> (Router, DeviceId, ClipId) {
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let id = ClipId {
            origin: peer,
            seq: DND_SEQ_BIT | 1,
        };
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: me,
                    nonce: [me.0[0]; 16],
                },
                "local",
                Os::Other,
            ),
            crate::router::connection_tests::session_layout(me, peer),
        );
        r.set_dnd_available(true);
        r.test_connect(peer, 1);
        enter_target(&mut r, peer, id, 2, 2, incomplete);
        r.out.clear();
        (r, peer, id)
    }

    #[test]
    fn optional_preview_never_delays_target_begin() {
        let (r, _, id) = preview_wait();
        assert!(matches!(r.dnd.state, State::Hosting { id: active, .. } if active == id));
        assert!(r.dnd.pending.is_none());
    }

    #[test]
    fn late_preview_readiness_requests_low_bytes_without_restarting_host() {
        let (mut r, peer, id) = preview_wait();
        let mut notice = r.dnd.notices[&peer].notice.clone();
        notice.revision += 1;
        notice.preview = PreviewAvailability::Available { revision: 2 };
        r.on_dnd_notice(peer, notice.clone(), false, 5);
        assert!(r.out.iter().any(|o| matches!(o, Output::Send { msg: Message::Dnd(DndMsg::PreviewRequest { binding, .. }), .. } if binding.preview_revision == 2)));
        assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
        r.out.clear();
        r.on_dnd_msg(
            peer,
            DndMsg::Preview {
                id,
                epoch: 2,
                binding: PreviewBinding {
                    key: notice.key,
                    metadata_revision: notice.metadata.revision,
                    preview_revision: 2,
                },
                image: Some(preview_image(2.)),
            },
        );
        assert!(
            r.out
                .iter()
                .any(|o| matches!(o, Output::Dnd(DndCmd::UpdateImage { .. })))
        );
    }

    #[test]
    fn source_loss_before_preview_must_not_start_a_late_host() {
        let (mut r, peer, id) = preview_wait();
        r.on_dnd_msg(
            peer,
            DndMsg::Refused {
                id,
                holder: Holder {
                    device: r.me(),
                    epoch: 2,
                },
                reason: RevokeReason::CatcherLost,
            },
        );
        r.on_dnd_msg(
            peer,
            DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 2,
                image: None,
            },
        );
        assert!(r.dnd.pending.is_none());
        assert_eq!(r.dnd_host_pending(), None);
        assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
    }

    #[test]
    fn later_host_gets_source_preview_without_confusing_the_focus_sender() {
        let (mut r, previous, _) = preview_wait();
        let source = DeviceId([3; 32]);
        let id = ClipId {
            origin: source,
            seq: DND_SEQ_BIT | 7,
        };
        r.test_connect(source, 3);
        r.out.clear();
        enter_preview_wait(&mut r, previous, id, 3, 4);
        assert!(r.out.iter().any(|o| matches!(o,
            Output::Send { to, msg: Message::Dnd(DndMsg::PreviewRequest { id: found, epoch: 3,.. }), .. }
            if *to == source && *found == id)));
        assert!(!r.out.iter().any(|o| matches!(o,
            Output::Send { to, msg: Message::Dnd(DndMsg::PreviewRequest { .. }), .. }
            if *to == previous)));
        r.out.clear();
        let image = preview_image(2.);
        r.on_dnd_msg(
            previous,
            DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 3,
                image: Some(image.clone()),
            },
        );
        assert!(r.dnd.pending.is_none());
        assert!(
            !r.out
                .iter()
                .any(|o| matches!(o, Output::Dnd(DndCmd::UpdateImage { .. })))
        );
        r.on_dnd_msg(
            source,
            DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 2,
                image: Some(image.clone()),
            },
        );
        assert!(r.dnd.pending.is_none());
        assert!(
            !r.out
                .iter()
                .any(|o| matches!(o, Output::Dnd(DndCmd::UpdateImage { .. })))
        );
        r.on_dnd_msg(
            source,
            DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 3,
                image: Some(image.clone()),
            },
        );
        assert!(r.dnd.pending.is_none());
        assert!(r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::UpdateImage { id: found, epoch: 3, image: got }) if *found == id && *got == image)));
    }

    #[test]
    fn held_and_exported_drag_input_disconnect_cannot_consume_the_next_click_up() {
        for exported in [false, true] {
            for receipt_order in if exported { 0..3 } else { 1..3 } {
                let me = DeviceId([1; 32]);
                let input = DeviceId([2; 32]);
                let mut r = Router::new(
                    RouterConfig::new(
                        oink_foundation::session::Boot {
                            device_id: me,
                            nonce: [1; 16],
                        },
                        "source",
                        Os::MacOS,
                    ),
                    crate::router::connection_tests::session_layout(me, input),
                );
                r.set_dnd_available(true);
                r.test_connect(input, 1);
                let id = ClipId {
                    origin: me,
                    seq: DND_SEQ_BIT | 1,
                };
                let offer = Router::dnd_offer(id);
                let local = Holder {
                    device: me,
                    epoch: r.epoch,
                };
                let mut source = source::Source::new(offer, input, local);
                if exported {
                    assert!(source.export_ready());
                }
                assert!(source.handoff(
                    local,
                    Holder {
                        device: input,
                        epoch: r.epoch + 1
                    }
                ));
                r.dnd.sources.insert(offer, source);
                r.dnd_take_source_press(offer, input);
                if exported {
                    assert!(r.dnd.presses.get_mut(&offer).unwrap().submit());
                } else {
                    r.dnd.native_source = Some(offer);
                }
                if receipt_order == 0 {
                    r.on_dnd_event(DndEvent::SourceButtonReleased { id, ok: true });
                }
                // The input link fails before its physical UP. Exported sources
                // already submitted native UP; held sources submit it on loss.
                // The receipt may follow reconnection and a fresh injected DOWN.
                r.test_disconnect(input, 2);
                if !exported {
                    assert!(r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::FinishSource { id: found, accept: false, physical: false, .. }) if *found == id)), "the lost injected input still owes one native source UP");
                }
                if receipt_order == 1 {
                    r.on_dnd_event(DndEvent::SourceButtonReleased { id, ok: true });
                }
                r.test_connect(input, 3);
                if !exported {
                    r.on_dnd_event(DndEvent::SourceEnded { id, accepted: false });
                }
                r.holder = me;
                r.claim = me;
                r.epoch = 3;
                r.focus_scope = Some(r.new_focus_scope());
                r.out.clear();
                for pressed in [true, false] {
                    r.test_message(
                        input,
                        Message::Input(InputMsg::Button {
                            button: MouseButton::Left,
                            pressed,
                        }),
                        4,
                    );
                    if pressed && receipt_order == 2 {
                        r.on_dnd_event(DndEvent::SourceButtonReleased { id, ok: true });
                    }
                }
                let buttons: Vec<_> = r
                    .out
                    .iter()
                    .filter_map(|o| match o {
                        Output::Inject(oink_foundation::input::InjectEvent::Button {
                            button: MouseButton::Left,
                            pressed,
                        }) => Some(*pressed),
                        _ => None,
                    })
                    .collect();
                assert_eq!(buttons, [true, false], "old release debt swallowed a fresh click's UP");
                assert!(r.held.get(&input).is_none_or(|h| h.buttons.is_empty()));
            }
        }
    }

    #[test]
    fn windows_catcher_loss_preserves_the_original_physical_up_obligation() {
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: me,
                    nonce: [me.0[0]; 16],
                },
                "source",
                Os::Windows,
            ),
            crate::router::connection_tests::session_layout(me, peer),
        );
        let id = ClipId {
            origin: me,
            seq: DND_SEQ_BIT | 1,
        };
        let offer = Router::dnd_offer(id);
        let local = Holder { device: me, epoch: 1 };
        let mut source = source::Source::new(offer, me, local);
        assert!(source.handoff(local, Holder { device: peer, epoch: 2 }));
        r.dnd.sources.insert(offer, source);
        r.dnd_take_source_press(offer, r.dnd.sources[&offer].input);
        r.dnd.native_source = Some(offer);
        r.buttons.insert(MouseButton::Left, Target::Remote(peer));
        r.holder = peer;
        r.epoch = 2;
        // A Windows IDropTarget DragLeave loses the catcher, not the physical
        // DOWN previously delivered to the browser. No FinishSource/UP ran.
        r.on_dnd_event(DndEvent::SourceCatcherLost { id });
        r.holder = me;
        r.epoch = 3;
        r.out.clear();
        assert_eq!(
            r.local_button(MouseButton::Left, false),
            oink_foundation::input::Disposition::Pass
        );
        assert!(r.buttons.is_empty());
        assert!(r.button_refs.is_empty());
        assert!(r.dnd.native_source.is_none());
        assert_eq!(r.dnd_state_name(), "idle");
        assert!(
            !r.out
                .iter()
                .any(|o| matches!(o, Output::Inject(_) | Output::Send { .. }))
        );
    }

    #[test]
    fn file_url_preparation_ends_on_up_escape_leave_or_failure_and_cancels_the_source_once() {
        for ending in 0..4 {
            let (mut r, peer, id) = preview_wait();
            r.test_message(
                peer,
                Message::Dnd(DndMsg::Preview {
                    binding: preview_binding(id),
                    id,
                    epoch: 2,
                    image: None,
                }),
                4,
            );
            assert_eq!(r.dnd_host_pending(), Some((id, 2)));
            r.out.clear();
            match ending {
                0 => release_notice(&mut r, peer, id, 2, 5),
                1 => {
                    r.dnd_escape(peer);
                }
                2 => r.dnd_focus_left(),
                _ => {} // manager failure/watchdog while still holding Left
            }
            if ending != 3 {
                assert_eq!(r.dnd_host_pending(), None);
            }
            // Failed Begin uses the existing host result, never the pre-Enter
            // pending-entry cancellation path. This delivers Cancel to the held
            // native source even if the target native session never existed.
            r.on_dnd_event(DndEvent::Ended {
                id,
                epoch: 2,
                dropped: false,
            });
            r.on_dnd_event(DndEvent::Ended {
                id,
                epoch: 2,
                dropped: false,
            });
            assert_eq!(r.dnd_host_pending(), None);
            assert_eq!(
                r.out
                    .iter()
                    .filter(|o| matches!(o, Output::Send { to, msg: Message::Dnd(DndMsg::Source {
                offer, action: SourceAction::Cancel, ..
            }), .. } if *to == peer && offer.clip == id))
                    .count(),
                1
            );
            assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
        }
    }

    #[test]
    fn released_native_drop_survives_focus_return_until_its_keyed_end() {
        for dropped in [true, false] {
            let (mut r, peer, id) = preview_wait();
            r.on_dnd_msg(
                peer,
                DndMsg::Preview {
                    binding: preview_binding(id),
                    id,
                    epoch: 2,
                    image: None,
                },
            );
            let grant = r.dnd.target.clone().unwrap();
            assert!(grant.claim_down());
            grant.down_submitted(true);
            release_notice(&mut r, peer, id, 2, 5);
            assert!(grant.claim_up());
            r.out.clear();

            // AppKit may still be completing a Dock drop while the released
            // pointer has already crossed back to its input device.
            r.become_source(3, peer);
            assert!(grant.content_current(), "focus return revoked the pending drop");
            assert!(
                !r.out
                    .iter()
                    .any(|o| matches!(o, Output::Dnd(DndCmd::Cancel { .. } | DndCmd::RevokePreparation { .. })))
            );
            r.out.clear();
            r.on_dnd_event(DndEvent::Ended {
                id,
                epoch: 3,
                dropped: true,
            });
            assert!(r.out.is_empty(), "an unrelated focus epoch settled the old host");
            r.on_dnd_event(DndEvent::Ended { id, epoch: 2, dropped });
            assert_eq!(r.dnd_state_name(), "idle");
            assert_eq!(grant.content_current(), dropped);
            let action = if dropped {
                SourceAction::StartDelivery
            } else {
                SourceAction::Cancel
            };
            assert_eq!(
                r.out
                    .iter()
                    .filter(|o| matches!(o,
                        Output::Send { to, msg: Message::Dnd(DndMsg::Source {
                            offer, holder, action: found, ..
                        }), .. } if *to == peer && offer.clip == id && holder.epoch == 2 && *found == action
                    ))
                    .count(),
                1
            );
            r.out.clear();
            r.on_dnd_event(DndEvent::Ended { id, epoch: 2, dropped });
            assert!(r.out.is_empty(), "duplicate native end repeated delivery");
        }
    }

    #[test]
    fn escape_and_explicit_cancel_after_release_cannot_undo_the_drop() {
        for return_focus in [false, true] {
            let (mut r, peer, id) = preview_wait();
            r.on_dnd_msg(
                peer,
                DndMsg::Preview {
                    binding: preview_binding(id),
                    id,
                    epoch: 2,
                    image: None,
                },
            );
            let grant = r.dnd.target.clone().unwrap();
            assert!(grant.claim_down());
            grant.down_submitted(true);
            release_notice(&mut r, peer, id, 2, 5);
            if return_focus {
                r.become_source(3, peer);
            }
            r.out.clear();
            assert!(r.dnd_escape(peer), "consume Esc while native completion is pending");
            assert!(
                r.dnd_escape(r.me()),
                "the target keyboard cannot undo a released drop either"
            );
            r.cancel_dnd();
            r.dnd_local_press();
            assert!(grant.content_current());
            assert!(r.out.is_empty(), "post-release input cancelled a completed drop");
            r.on_dnd_event(DndEvent::Ended {
                id,
                epoch: 2,
                dropped: true,
            });
            assert_eq!(r.dnd_state_name(), "idle");
            assert!(grant.content_current());
            assert!(r.out.iter().any(|o| matches!(
                o,
                Output::Send {
                    msg: Message::Dnd(DndMsg::Source {
                        holder: Holder { epoch: 2, .. },
                        action: SourceAction::StartDelivery,
                        ..
                    }),
                    ..
                }
            )));
        }
    }

    #[test]
    fn source_escape_after_physical_release_preserves_pending_delivery() {
        let (mut r, peer, id) = preview_wait();
        r.dnd.state = State::Idle;
        let offer = Router::dnd_offer(ClipId {
            origin: r.me(),
            seq: id.seq,
        });
        let mut source = source::Source::new(offer, r.me(), Holder { device: peer, epoch: 2 });
        source.phase = source::Phase::Held;
        source.confirmed = true;
        source.native = NativeObservation::Held;
        assert!(!source.released());
        r.dnd.sources.insert(offer, source);
        r.dnd.native_source = Some(offer);
        r.out.clear();
        assert!(r.dnd_escape(r.me()), "Esc must not reach the still-held native source");
        r.holder = peer;
        assert_eq!(
            r.local_key(oink_foundation::input::KeyCode::ESCAPE, true, 6),
            oink_foundation::input::Disposition::Swallow,
            "the input device must consume Esc even while focus is remote"
        );
        r.local_key(oink_foundation::input::KeyCode::ESCAPE, false, 7);
        r.cancel_dnd();
        r.dnd_local_press();
        assert_eq!(r.dnd.sources[&offer].phase, source::Phase::Released);
        assert!(r.out.is_empty());
    }

    #[test]
    fn escape_before_release_still_cancels_native_host() {
        let (mut r, peer, id) = preview_wait();
        r.on_dnd_msg(
            peer,
            DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 2,
                image: None,
            },
        );
        let grant = r.dnd.target.clone().unwrap();
        assert!(grant.claim_down());
        grant.down_submitted(true);
        r.out.clear();
        assert!(r.dnd_escape(peer));
        assert!(!grant.content_current());
        assert!(r.out.iter().any(|o| matches!(o,
            Output::Dnd(DndCmd::Cancel { id: found, epoch: 2, .. }) if *found == id
        )));
    }

    #[test]
    fn leaving_before_native_host_exists_cancels_held_source_once() {
        let (mut r, peer, id) = preview_wait();
        r.test_message(
            peer,
            Message::Dnd(DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 2,
                image: None,
            }),
            4,
        );
        r.dnd.state = State::Leaving {
            epoch: 2,
            input: peer,
            gesture: EdgeGesture::fixture(r.me(), peer, Point::new(1., 1.), (1., 0.), 0.),
            id,
            items: Vec::new(),
            native: false,
            released: false,
        };
        r.out.clear();
        r.on_dnd_event(DndEvent::Ended {
            id,
            epoch: 2,
            dropped: false,
        });
        for _ in 0..2 {
            r.on_dnd_event(DndEvent::HostLeft {
                id,
                epoch: 2,
                ok: false,
            });
        }
        assert_eq!(
            r.out
                .iter()
                .filter(|o| matches!(o, Output::Send { to, msg: Message::Dnd(DndMsg::Source {
            offer, action: SourceAction::Cancel, ..
        }), .. } if *to == peer && offer.clip == id))
                .count(),
            1
        );
        assert_eq!(r.dnd_host_pending(), None);
    }

    #[test]
    fn receiver_accepts_512_kib_preview_and_rejects_larger_png() {
        for (bytes, accepted) in [(512 * 1024, true), (512 * 1024 + 1, false)] {
            let (mut r, peer, id) = preview_wait();
            let mut image = preview_image(1.);
            std::sync::Arc::make_mut(&mut image.png).resize(bytes, 0);
            r.test_message(
                peer,
                Message::Dnd(DndMsg::Preview {
                    binding: preview_binding(id),
                    id,
                    epoch: 2,
                    image: Some(image.clone()),
                }),
                4,
            );
            assert_eq!(
                r.out
                    .iter()
                    .any(|o| matches!(o, Output::Dnd(DndCmd::UpdateImage { image: got, .. }) if *got == image)),
                accepted,
            );
            assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
        }
    }

    fn enter_preview_wait(r: &mut Router, peer: DeviceId, id: ClipId, epoch: u64, now: u64) {
        enter_target(r, peer, id, epoch, now, false);
    }
    fn enter_target(r: &mut Router, peer: DeviceId, id: ClipId, epoch: u64, now: u64, incomplete: bool) {
        r.on_dnd_notice(
            id.origin,
            offered(r, id, epoch, peer, vec![ClipData::Text("drag".into())]),
            incomplete,
            now,
        );
        r.test_message(peer, Message::Dnd(DndMsg::Enter { id, epoch }), now);
        r.test_message(
            peer,
            Message::Control(crate::control::ControlMsg::FocusEnter {
                epoch,
                display: "screen".into(),
                pos: Point::new(2., 3.),
                scope: Some(r.new_focus_scope()),
                pointer_source: Some(peer),
            }),
            now + 1,
        );
        if !incomplete {
            let commands: Vec<_> = r
                .out
                .iter()
                .filter_map(|o| if let Output::Dnd(c) = o { Some(c) } else { None })
                .collect();
            let begin = commands.iter().position(|c| matches!(c, DndCmd::Begin { .. })).unwrap();
            assert!(begin > 0 && matches!(commands[begin - 1], DndCmd::SetImage(None)));
        }
        assert_eq!(r.dnd.pending.is_some(), incomplete);
        assert_eq!(
            r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))),
            !incomplete
        );
    }

    #[test]
    fn missing_preview_begins_immediately_and_refusal_closes_host() {
        let (mut r, peer, id) = preview_wait();
        assert_eq!(r.dnd_deadline(), None);
        r.dnd.now = 100_000_000;
        r.dnd_expire();
        assert!(r.dnd.pending.is_none());
        assert!(matches!(r.dnd.state, State::Hosting { .. }));
        r.on_dnd_msg(
            peer,
            DndMsg::Refused {
                id,
                holder: Holder {
                    device: r.me(),
                    epoch: 2,
                },
                reason: RevokeReason::PreparationFailed,
            },
        );
        r.on_dnd_msg(
            peer,
            DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 2,
                image: Some(preview_image(1.)),
            },
        );
        assert!(r.dnd.pending.is_none());
        assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
    }

    #[test]
    fn pending_without_native_down_never_injects_up_on_release_cancel_or_revocation() {
        use oink_foundation::input::InjectEvent;
        for ending in 0..5 {
            let (mut r, peer, id) = metadata_wait();
            match ending {
                0 => release_notice(&mut r, peer, id, 2, 20),
                1 => {
                    r.dnd_escape(peer);
                }
                2 => r.on_dnd_refused(peer, id, 2),
                3 => r.test_disconnect(peer, 20),
                _ => r.dnd_focus_left(),
            }
            r.on_dnd_msg(
                peer,
                DndMsg::Preview {
                    binding: preview_binding(id),
                    id,
                    epoch: 2,
                    image: None,
                },
            );
            assert!(r.dnd.pending.is_none());
            assert!(!r.out.iter().any(|o| matches!(
                o,
                Output::Dnd(DndCmd::Begin { .. }) | Output::Inject(InjectEvent::Button { .. })
            )));
        }
    }
    #[test]
    fn local_up_during_preview_wait_keeps_the_original_source_route() {
        let (mut r, peer, id) = metadata_wait();
        let me = r.me();
        r.dnd.pending.as_mut().unwrap().input = me;
        r.buttons.insert(MouseButton::Left, Target::Remote(peer));
        r.local_button(MouseButton::Left, false);
        assert!(r.out.iter().any(|o| matches!(o,Output::Send{to,msg:Message::Input(InputMsg::Button{button:MouseButton::Left,pressed:false}),..} if *to==peer)));
        release_notice(&mut r, peer, id, 2, 20);
        assert!(!r.buttons.contains_key(&MouseButton::Left));
        assert!(!r.button_refs.contains_key(&MouseButton::Left));
        assert!(
            !r.out
                .iter()
                .any(|o| matches!(o, Output::Inject(_) | Output::Dnd(DndCmd::Begin { .. })))
        );
    }

    #[test]
    fn preview_requires_the_current_native_metadata_and_artwork_revisions() {
        for field in 0..3 {
            let (mut r, peer, id) = preview_wait();
            let mut binding = preview_binding(id);
            match field {
                0 => binding.key.native_generation += 1,
                1 => binding.metadata_revision += 1,
                _ => binding.preview_revision += 1,
            }
            r.on_dnd_msg(
                peer,
                DndMsg::Preview {
                    id,
                    epoch: 2,
                    binding,
                    image: Some(preview_image(1.)),
                },
            );
            assert!(r.dnd.pending.is_none());
            assert!(
                !r.out
                    .iter()
                    .any(|o| matches!(o, Output::Dnd(DndCmd::UpdateImage { .. })))
            );
            assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
        }
    }
    #[test]
    fn new_handoff_rejects_old_preview_even_for_the_same_drag_id() {
        let (mut r, peer, id) = preview_wait();
        enter_preview_wait(&mut r, peer, id, 3, 4);
        r.out.clear();
        r.test_message(
            peer,
            Message::Dnd(DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 2,
                image: Some(preview_image(1.)),
            }),
            6,
        );
        assert!(r.dnd.pending.is_none());
        assert!(
            !r.out
                .iter()
                .any(|o| matches!(o, Output::Dnd(DndCmd::UpdateImage { .. })))
        );
        assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
        r.test_message(
            peer,
            Message::Dnd(DndMsg::Preview {
                binding: preview_binding(id),
                id,
                epoch: 3,
                image: Some(preview_image(2.)),
            }),
            7,
        );
        assert!(r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::UpdateImage { id: found, epoch: 3, image }) if *found == id && image == &preview_image(2.))));
    }
}
