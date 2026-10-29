//! Source authentication, monotone state and Enter/Offered pairing. A focus
//! predecessor cannot manufacture a drag grant or reset a source tombstone.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct Received {
    pub notice: DragNotice,
    pub incomplete: bool,
    pub source_session: u64,
}
impl Router {
    pub(super) fn dnd_retire_preparation(&mut self, source: DeviceId) {
        if let Some(preparation) = self.dnd.preparations.remove(&source) {
            preparation.revoke();
            self.out.push(Output::Dnd(DndCmd::RevokePreparation { preparation }));
        }
    }
    fn dnd_prepare_notice(&mut self, notice: &DragNotice, session: u64) {
        let source = notice.key.offer.clip.origin;
        if notice.metadata.readiness != MetadataReadiness::Ready || self.dnd.preparations.contains_key(&source) {
            return;
        }
        let preparation = DndPreparation::new(notice.preparation(), session);
        let roots = notice
            .items
            .iter()
            .filter_map(|item| match item {
                ClipData::FileRoots(roots) => Some(roots.clone()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        self.dnd.preparations.insert(source, preparation.clone());
        if !roots.is_empty() {
            self.out.push(Output::Dnd(DndCmd::Prepare { preparation, roots }));
        }
    }
    pub fn on_dnd_notice(&mut self, from: DeviceId, notice: DragNotice, incomplete: bool, now: u64) {
        if !notice.valid(from) {
            return;
        }
        let id = notice.key.offer.clip;
        // Completion/revocation of an older independent delivery must still
        // resolve after a newer drag from the same source has been announced.
        if notice.state.terminal()
            && let Some(grant) = self.dnd.deliveries.remove(&notice.key)
        {
            if notice.revision >= grant.revision {
                grant.revoke();
            } else {
                self.dnd.deliveries.insert(notice.key, grant);
            }
        }
        if let Some(old) = self.dnd.notices.get(&from) {
            if old.notice.key != notice.key {
                if old.notice.key.native_generation >= notice.key.native_generation {
                    return;
                }
            } else if notice.revision < old.notice.revision
                || (notice.revision == old.notice.revision && (!old.incomplete || incomplete))
                || old.notice.state.terminal()
            {
                return;
            }
        } else if self.dnd.notices.len() >= 16 {
            if self.dnd.pending.as_ref().is_some_and(|p| p.id == id) {
                self.dnd_cancel_pending();
            }
            if let Some(holder) = notice.state.holder() {
                self.send(
                    from,
                    Message::Dnd(DndMsg::Source {
                        offer: notice.key.offer,
                        holder,
                        action: SourceAction::Cancel,
                        cancel_at: None,
                    }),
                );
            }
            return;
        }
        self.dnd.now = now;
        let session = self.dnd.source_sessions.entry(from).or_insert_with(|| {
            self.dnd.next_source_session = self
                .dnd
                .next_source_session
                .checked_add(1)
                .expect("source admission exhausted");
            self.dnd.next_source_session
        });
        let session = *session;
        let state = notice.state;
        // A completed optional preview can arrive after the host already began.
        // Request bytes on Low, without changing its native drag or pointer queue.
        if let DragState::Offered { holder, .. } = state
            && let PreviewAvailability::Available { revision } = notice.preview
            && self.dnd.target.as_ref().is_some_and(|g| {
                g.preparation.key == notice.preparation()
                    && g.preparation.source_session == session
                    && g.holder == holder
                    && g.begin_current()
            })
            && matches!(self.dnd.state, State::Hosting { id: active, .. } if active == id)
        {
            let binding = PreviewBinding {
                key: notice.key,
                metadata_revision: notice.metadata.revision,
                preview_revision: revision,
            };
            let request = (from, id, holder.epoch, binding);
            if self.dnd.preview_requested != Some(request) {
                self.dnd.preview_requested = Some(request);
                self.send(
                    from,
                    Message::Dnd(DndMsg::PreviewRequest {
                        id,
                        epoch: holder.epoch,
                        binding,
                    }),
                );
            }
        }
        if self
            .dnd
            .preparations
            .get(&from)
            .is_some_and(|p| p.key != notice.preparation() || p.source_session != session || state.terminal())
            && let Some(old) = self.dnd.preparations.remove(&from)
        {
            old.revoke();
            self.out
                .push(Output::Dnd(DndCmd::RevokePreparation { preparation: old }));
        }
        if state.terminal() {
            if let Some(grant) = self
                .dnd
                .target
                .as_ref()
                .filter(|g| g.preparation.key.drag == notice.key)
            {
                grant.revoke();
            }
        } else if !incomplete && matches!(state, DragState::Announced | DragState::Offered { .. }) {
            self.dnd_prepare_notice(&notice, session);
        }
        if let DragState::Offered { holder, .. } = state
            && self
                .dnd
                .target
                .as_ref()
                .is_some_and(|g| g.preparation.key.drag == notice.key && g.holder != holder)
        {
            self.dnd_cancel_hosting();
        }
        self.dnd.notices.insert(
            from,
            Received {
                notice,
                incomplete,
                source_session: session,
            },
        );
        if matches!(
            state,
            DragState::Revoked {
                scope: RevokeScope::Preparation,
                ..
            }
        ) || (matches!(
            state,
            DragState::Revoked {
                scope: RevokeScope::Holder(_),
                ..
            }
        ) && !self
            .dnd
            .target
            .as_ref()
            .is_some_and(|g| g.preparation.key.drag.offer.clip == id)
            && !self.dnd.pending.as_ref().is_some_and(|p| p.id == id))
        {
            self.dnd_retire_preparation(from);
            return;
        }
        if state.terminal()
            || matches!(state, DragState::Revoked { scope: RevokeScope::Holder(Holder { device, epoch }), .. } if device == self.me() && epoch == self.epoch)
        {
            if self.dnd.pending.as_ref().is_some_and(|p| p.id == id) {
                self.dnd_cancel_pending();
            }
            if matches!(self.dnd.state, State::Hosting { id: active, .. } if active == id) {
                self.dnd_cancel_hosting();
            }
            return;
        }
        if let DragState::Released { holder, .. } = state {
            if holder.device == self.me() {
                if self
                    .dnd
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.id == id && p.epoch == holder.epoch)
                {
                    self.dnd_cancel_pending();
                }
                if let State::Hosting {
                    id: active,
                    input,
                    begin_failed,
                    ..
                } = self.dnd.state
                    && active == id
                    && self.epoch == holder.epoch
                {
                    if begin_failed {
                        self.dnd_cancel_hosting();
                        return;
                    }
                    if let Some(grant) = &self.dnd.target {
                        grant.release();
                    }
                    self.out.push(Output::Dnd(DndCmd::PrepareDrop {
                        id,
                        epoch: holder.epoch,
                    }));
                    self.release_button_silently(input, MouseButton::Left);
                }
            } else {
                // Group preparation never becomes a content grant. A member
                // that was never entered has no later native callback to clean it.
                self.dnd_retire_preparation(from);
            }
            return;
        }
        self.dnd_pair_notice();
    }
    pub fn on_dnd_attachment_refused(&mut self, from: DeviceId, key: DragKey, revision: u64) {
        // A transport refusal is explicit; it never silently turns into a drag.
        if from == key.offer.clip.origin
            && self
                .dnd
                .notices
                .get(&from)
                .is_some_and(|n| n.notice.key == key && n.notice.revision == revision)
        {
            if self.dnd.pending.as_ref().is_some_and(|p| p.id == key.offer.clip) {
                self.dnd_cancel_pending();
            }
        } else if key.offer.clip.origin == self.me()
            && let Some(source) = self
                .dnd
                .sources
                .get(&key.offer)
                .filter(|s| s.key() == key && s.holder.device == from && s.revision == revision)
        {
            let holder = source.holder;
            self.dnd_source_action(from, key.offer, holder, SourceAction::Cancel, None);
        }
    }
    pub(super) fn dnd_pair_notice(&mut self) {
        let Some(p) = &mut self.dnd.pending else {
            return;
        };
        let Some(received) = self
            .dnd
            .notices
            .get(&p.id.origin)
            .filter(|n| n.notice.key.offer.clip == p.id)
        else {
            return;
        };
        let notice = &received.notice;
        if notice.state.terminal()
            || matches!(notice.state, DragState::Released { holder, .. } | DragState::Revoked { scope: RevokeScope::Holder(holder), .. } if holder.device == self.cfg.device && holder.epoch == p.epoch)
        {
            self.dnd_cancel_pending();
            return;
        }
        let DragState::Offered { holder, .. } = notice.state else {
            return;
        };
        if holder.device != self.cfg.device || holder.epoch != p.epoch {
            return;
        }
        if notice.metadata.readiness != MetadataReadiness::Ready {
            return;
        }
        p.preview_binding = Some(PreviewBinding {
            key: notice.key,
            metadata_revision: notice.metadata.revision,
            preview_revision: match notice.preview {
                PreviewAvailability::Available { revision } => revision,
                _ => 0,
            },
        });
        p.retained_inventory = notice.retained_inventory.clone();
        p.input = notice.input;
        p.items = notice.items.clone();
        p.incomplete = received.incomplete;
        p.grant = Some((notice.key, notice.revision, received.source_session));
        if matches!(notice.preview, PreviewAvailability::Absent) {
            p.preview_ready = true;
        }
        let (from, epoch, focused) = (p.from, p.epoch, p.focus_entered);
        let (notice, session, complete) = (notice.clone(), received.source_session, !received.incomplete);
        if complete {
            self.dnd_prepare_notice(&notice, session);
        }
        if focused {
            let pos = self.pointer.as_ref().map(|p| p.local).unwrap_or_default();
            self.dnd_focus_entered(from, epoch, pos);
        }
    }
    // A valid future NoticeRequest waits for the predecessor's Handoff.
    // It needs no recipient bookkeeping: the whole group receives terminal state.
    pub(super) fn dnd_notice_request(&mut self, from: DeviceId, id: ClipId, holder: Holder, previous: DeviceId) {
        if id.origin != self.me() || from != holder.device {
            return;
        }
        let offer = Self::dnd_offer(id);
        let Some(source) = self.dnd.sources.get_mut(&offer) else {
            self.send(
                from,
                Message::Dnd(DndMsg::Refused {
                    id,
                    holder,
                    reason: RevokeReason::SourceUnavailable,
                }),
            );
            return;
        };
        if source.holder == holder
            && source.confirmed
            && matches!(source.phase, source::Phase::Local | source::Phase::Held)
        {
            self.dnd_publish_offer(offer);
        } else if !(holder.epoch > source.holder.epoch
            && previous == source.holder.device
            && matches!(source.phase, source::Phase::Local | source::Phase::Held))
        {
            self.send(
                from,
                Message::Dnd(DndMsg::Refused {
                    id,
                    holder,
                    reason: RevokeReason::InvalidTransition,
                }),
            );
        }
    }
    pub(super) fn dnd_preview_availability(&self, id: ClipId) -> PreviewAvailability {
        if self.dnd.image.as_ref().is_some_and(|(found, _)| *found == id) {
            PreviewAvailability::Available { revision: 1 }
        } else {
            PreviewAvailability::Absent
        }
    }
    pub(in crate::router) fn dnd_member_attached(&mut self, peer: DeviceId) {
        let notices: Vec<_> = self
            .dnd
            .sources
            .values()
            .filter(|s| matches!(s.phase, source::Phase::Local | source::Phase::Held))
            .map(|s| s.notice(DragState::Announced, PreviewAvailability::Unknown))
            .collect();
        // Only the newly attached member needs a snapshot. Reannouncing to an
        // existing holder would overwrite its Offered state without a new grant.
        for notice in notices {
            self.send(peer, Message::Dnd(DndMsg::Notice(notice)));
        }
    }
    pub(super) fn dnd_announce_members(&mut self, offer: OfferId) {
        let Some(source) = self.dnd.sources.get_mut(&offer) else {
            return;
        };
        source.advance();
        let notice = source.notice(DragState::Announced, PreviewAvailability::Unknown);
        self.broadcast(Message::Dnd(DndMsg::Notice(notice)));
    }
    pub(super) fn dnd_publish_offer(&mut self, offer: OfferId) {
        let preview = self.dnd_preview_availability(offer.clip);
        let Some(source) = self
            .dnd
            .sources
            .get_mut(&offer)
            .filter(|s| s.confirmed && s.exporting.is_none())
        else {
            return;
        };
        // The target now receives the group Announced too. Its Offered must
        // supersede that state, including when a repeated request republishes it.
        source.advance();
        let notice = source.notice(
            DragState::Offered {
                holder: source.holder,
                mode: source.mode(),
            },
            preview,
        );
        let to = source.holder.device;
        if to == self.me() {
            self.on_dnd_notice(to, notice, false, self.dnd.now);
        } else {
            self.send(to, Message::Dnd(DndMsg::Notice(notice)));
        }
    }
    pub(super) fn dnd_publish_state(&mut self, offer: OfferId, state: DragState) {
        let Some(source) = self.dnd.sources.get(&offer) else {
            return;
        };
        let notice = source.notice(state, self.dnd_preview_availability(offer.clip));
        let local = source.holder.device == self.me();
        self.broadcast(Message::Dnd(DndMsg::Notice(notice.clone())));
        if local {
            self.on_dnd_notice(self.me(), notice, false, self.dnd.now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn held_mac_promise_is_exported_before_a_later_windows_offer() {
        use oink_foundation::clipboard::ClipPlatform;
        use oink_foundation::content::*;
        let me = DeviceId([1; 32]);
        let mac = DeviceId([2; 32]);
        let windows = DeviceId([3; 32]);
        let id = ClipId {
            origin: me,
            seq: DND_SEQ_BIT | 1,
        };
        let offer = Router::dnd_offer(id);
        for (success, moved, released) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let mut r = Router::new(
                crate::RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: me,
                        nonce: [1; 16],
                    },
                    "source",
                    crate::Os::MacOS,
                ),
                crate::router::connection_tests::session_layout(me, mac),
            );
            r.test_connect(mac, 1);
            r.test_connect(windows, 1);
            r.peers.get_mut(&windows).unwrap().os = crate::Os::Windows;
            let previous = Holder { device: mac, epoch: 2 };
            let next = Holder {
                device: windows,
                epoch: 3,
            };
            let mut source = source::Source::new(offer, me, previous);
            source.phase = source::Phase::Held;
            source.confirmed = true;
            source.metadata(vec![ClipData::Native {
                platform: ClipPlatform::Mac,
                item: ContentItem {
                    id: ItemId(1),
                    file: None,
                    virtual_files: true,
                    representations: vec![Representation {
                        id: RepresentationId(1),
                        format: NativeFormat::Mac("com.apple.pasteboard.promised-file-url".into()),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard: None,
                        data: RepresentationData::Promise(NodeId(1), vec![]),
                    }],
                },
            }]);
            r.dnd.sources.insert(offer, source);
            r.dnd.native_source = Some(offer);
            r.drain_outputs();
            r.dnd_source_action(mac, offer, previous, SourceAction::Handoff { next }, None);
            // A repeat request must not expose the pre-export promise either.
            r.dnd_notice_request(windows, id, next, mac);
            let offered = |out: &Output| {
                matches!(
                    out,
                    Output::Send {
                        msg: Message::Dnd(DndMsg::Notice(DragNotice {
                            state: DragState::Offered { .. },
                            ..
                        })),
                        ..
                    }
                )
            };
            let pending = r.drain_outputs();
            assert!(
                !pending.iter().any(offered),
                "Windows cannot use unresolved Mac promises"
            );
            assert_eq!(
                pending
                    .iter()
                    .filter(|output| matches!(output,
                        Output::Dnd(DndCmd::ExportSource { id: found, epoch: 3, physical: true }) if *found == id
                    ))
                    .count(),
                1,
                "handoff exports the original native source exactly once"
            );
            let files = vec![ClipData::Files(vec![oink_foundation::clipboard::FileEntry {
                path: "image.png".into(),
                is_dir: false,
                size: 42,
                modified_ms: None,
                local_path: None,
            }])];
            let final_holder = if moved {
                let returned = Holder { device: mac, epoch: 4 };
                r.dnd_source_action(windows, offer, next, SourceAction::Handoff { next: returned }, None);
                assert!(!r.drain_outputs().iter().any(offered));
                returned
            } else {
                next
            };
            if released {
                assert!(!r.dnd.sources.get_mut(&offer).unwrap().released());
            }
            r.on_dnd_event(DndEvent::SourceExported {
                id,
                epoch: next.epoch,
                items: if success {
                    Ok(files.clone())
                } else {
                    Err(oink_foundation::dnd::ExportFailure::Failed)
                },
            });
            let outputs = r.drain_outputs();
            assert_eq!(outputs.iter().filter(|o| offered(o)).count(), usize::from(success));
            if success {
                let source = &r.dnd.sources[&offer];
                assert_eq!(source.items, files);
                assert_eq!(source.holder, final_holder);
                assert!(source.exported);
                r.on_dnd_event(DndEvent::SourceExported {
                    id,
                    epoch: next.epoch,
                    items: Ok(files.clone()),
                });
                assert!(
                    r.dnd.sources.contains_key(&offer),
                    "duplicate export must not revoke the offer"
                );
                assert!(r.drain_outputs().is_empty());
            } else {
                assert!(!r.dnd.sources.contains_key(&offer));
            }
        }
    }

    #[test]
    fn terminal_before_enter_rejects_late_offer_in_both_link_orders() {
        let me = DeviceId([1; 32]);
        let source = DeviceId([2; 32]);
        let predecessor = DeviceId([3; 32]);
        let id = ClipId {
            origin: source,
            seq: DND_SEQ_BIT | 1,
        };
        for offer_before_enter in [false, true] {
            let mut r = Router::new(
                crate::RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: me,
                        nonce: [me.0[0]; 16],
                    },
                    "target",
                    crate::Os::MacOS,
                ),
                crate::router::connection_tests::session_layout(me, source),
            );
            r.set_dnd_available(true);
            r.test_connect(source, 1);
            r.test_connect(predecessor, 1);
            let holder = Holder { device: me, epoch: 2 };
            let offer = DragNotice {
                key: DragKey {
                    offer: Router::dnd_offer(id),
                    native_generation: 1,
                },
                revision: 1,
                input: predecessor,
                state: DragState::Offered {
                    holder,
                    mode: SourceMode::Held,
                },
                metadata: MetadataVersion {
                    revision: 1,
                    readiness: MetadataReadiness::Ready,
                },
                preview: PreviewAvailability::Absent,
                retained_inventory: None,
                items: vec![ClipData::Text("drag".into())],
            };
            let mut ended = offer.clone_without_items();
            ended.revision = 2;
            ended.state = DragState::Revoked {
                scope: RevokeScope::Drag,
                reason: RevokeReason::CatcherLost,
            };
            r.on_dnd_notice(source, ended, false, 2);
            if offer_before_enter {
                r.on_dnd_notice(source, offer.clone(), false, 3);
            }
            r.on_dnd_msg(predecessor, DndMsg::Enter { id, epoch: 2 });
            r.holder = me;
            r.epoch = 2;
            r.dnd_focus_entered(predecessor, 2, Point::new(2., 3.));
            if !offer_before_enter {
                r.on_dnd_notice(source, offer, false, 3);
            }
            r.on_dnd_msg(
                source,
                DndMsg::Preview {
                    binding: PreviewBinding {
                        key: DragKey {
                            offer: Router::dnd_offer(id),
                            native_generation: 1,
                        },
                        metadata_revision: 1,
                        preview_revision: 0,
                    },
                    id,
                    epoch: 2,
                    image: None,
                },
            );
            assert!(r.dnd.pending.is_none());
            assert!(!r.out.iter().any(|o| matches!(o, Output::Dnd(DndCmd::Begin { .. }))));
        }
    }
}
