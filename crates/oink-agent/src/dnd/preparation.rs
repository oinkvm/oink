use super::*;
use oink_clipboard::model::FileNode;
use oink_routing::DndPreparation;

#[cfg(target_os = "macos")]
pub(super) struct Slot {
    _update_activity: Option<oink_foundation::maintenance::Lease>,
    token: DndPreparation,
    roots: Vec<FileNode>,
    cancellation: oink_clipboard::file_provider::PreparationCancellation,
    choice: super::preparation_choice::Choice<
        Result<Arc<oink_clipboard::file_provider::PreparedDrag>, oink_clipboard::FileProviderReason>,
    >,
}

impl DndGlue {
    pub(super) fn release_target(&self, grant: &DndTarget, physical: bool) -> bool {
        if !grant.claim_up() {
            return true;
        }
        let result = self.submit_source_release(physical);
        if let Some(inner) = self.input_node.upgrade()
            && let Some(trace) = inner.input_trace.get()
        {
            trace.dnd_native("LeftButton", None, Some(false), result.is_ok());
        }
        result.is_ok()
    }
    pub(super) fn release_active_target(&self, physical: bool) -> Result<(), String> {
        let grant = self.active_target.lock().unwrap().as_ref().map(|(_, g)| g.clone());
        if grant.is_none_or(|grant| self.release_target(&grant, physical)) {
            Ok(())
        } else {
            Err("target UP submission unconfirmed".into())
        }
    }
    pub(super) fn prepare(&self, token: DndPreparation, roots: Vec<FileNode>) {
        #[cfg(not(target_os = "macos"))]
        let _ = (token, roots);
        #[cfg(target_os = "macos")]
        {
            if !self.backend.uses_file_provider() || !token.current() {
                return;
            }
            let mut preparations = self.preparations.lock().unwrap();
            let source = token.key.drag.offer.clip.origin;
            if let Some(inner) = self.input_node.upgrade() {
                oink_clipboard::received::remember_source(source, (crate::node::Node { inner }).device_name(source));
            }
            if preparations.get(&source).is_some_and(|slot| slot.token == token) {
                return;
            }
            if let Some(old) = preparations.remove(&source) {
                old.revoke();
            }
            let Some(activity) =
                oink_foundation::maintenance::process().enter(oink_foundation::maintenance::ActivityKind::Drag)
            else {
                return;
            };
            let slot = Arc::new(Slot::new(token, roots, Some(activity)));
            preparations.insert(source, slot.clone());
            self.runtime.spawn_blocking(move || {
                let result = oink_clipboard::file_provider::prepare_announced_with_cancel(
                    fp_key(&slot.token),
                    slot.roots.clone(),
                    &slot.cancellation,
                    &|| slot.token.current(),
                )
                .map(Arc::new)
                .map_err(|e| oink_clipboard::file_provider::failure_reason(&e));
                if !slot.token.current()
                    && let Ok(prepared) = &result
                {
                    oink_clipboard::file_provider::revoke_prepared(prepared);
                }
                slot.choice.complete(result);
            });
        }
    }
    pub(super) fn revoke_preparation(&self, token: DndPreparation) {
        token.revoke();
        #[cfg(target_os = "macos")]
        {
            let mut slots = self.preparations.lock().unwrap();
            let source = token.key.drag.offer.clip.origin;
            if slots.get(&source).is_some_and(|slot| slot.token == token)
                && let Some(slot) = slots.remove(&source)
            {
                slot.revoke();
            }
        }
    }
    #[cfg(target_os = "macos")]
    pub(super) fn preparation_activated(&self, grant: &DndTarget) {
        let mut slots = self.preparations.lock().unwrap();
        let source = grant.preparation.key.drag.offer.clip.origin;
        if slots.get(&source).is_some_and(|slot| slot.token == grant.preparation) {
            // Registration/readers now own the activated namespace. Dropping
            // this handle does not revoke late reads after a legitimate Drop.
            slots.remove(&source);
        }
    }
    #[cfg(target_os = "macos")]
    pub(super) fn prepared(
        &self,
        grant: &DndTarget,
        current: &dyn Fn() -> bool,
    ) -> Option<Result<Arc<oink_clipboard::file_provider::PreparedDrag>, oink_clipboard::FileProviderReason>> {
        let slot = self
            .preparations
            .lock()
            .unwrap()
            .get(&grant.preparation.key.drag.offer.clip.origin)
            .cloned()?;
        if slot.token != grant.preparation {
            return None;
        }
        let current = || grant.begin_current() && current();
        if !current() {
            return Some(Err(oink_clipboard::FileProviderReason::Cancelled));
        }
        Some(slot.choice.select(|| {
            let slot = slot.clone();
            let grant = grant.clone();
            let input_node = self.input_node.clone();
            // macOS 14/26 can take seconds to reconcile typed slot renames.
            // Keep the existing Announced publication running: its final URLs
            // may arrive first. Both workers have separate namespace identities;
            // only the first result can be activated, and the loser is retired.
            // This is preparation only; Begin and motion retain their own gates.
            self.runtime.spawn_blocking(move || {
                let current = || {
                    grant.begin_current()
                        && slot.choice.pending()
                        && input_node.upgrade().is_some_and(|inner| {
                            Node { inner }.input_status().is_ok_and(|status| {
                                !status.paused
                                    && status.dnd_host_pending
                                        == Some((grant.preparation.key.drag.offer.clip, grant.holder.epoch))
                            })
                        })
                };
                let result = oink_clipboard::file_provider::try_prepare_enter_placeholder(
                    oink_clipboard::file_provider::Activation {
                        key: fp_key(&slot.token),
                        holder: grant.holder,
                        grant_revision: grant.revision,
                    },
                    slot.roots.clone(),
                    &current,
                );
                let result = match result {
                    Ok(Some(prepared)) => Ok(Arc::new(prepared)),
                    Ok(None) => return,
                    Err(error) => Err(oink_clipboard::file_provider::failure_reason(&error)),
                };
                if slot.choice.complete(result) {
                    // Cancellation stops only the losing metadata helper; it
                    // never revokes the selected namespace or its content grant.
                    slot.cancellation.cancel();
                }
            });
        }))
    }
}
#[cfg(target_os = "macos")]
fn fp_key(token: &DndPreparation) -> oink_clipboard::file_provider::PreparationKey {
    oink_clipboard::file_provider::PreparationKey {
        offer: token.key.drag.offer,
        native_generation: token.key.drag.native_generation,
        source_session: token.source_session,
        metadata_revision: token.key.metadata_revision,
    }
}
#[cfg(target_os = "macos")]
impl Slot {
    fn new(token: DndPreparation, roots: Vec<FileNode>, activity: Option<oink_foundation::maintenance::Lease>) -> Self {
        Self {
            _update_activity: activity,
            token,
            roots,
            cancellation: oink_clipboard::file_provider::PreparationCancellation::new(),
            choice: super::preparation_choice::Choice::new(),
        }
    }
    fn revoke(&self) {
        self.token.revoke();
        self.cancellation.cancel();
        self.choice.with_result(|result| {
            if let Ok(prepared) = result {
                oink_clipboard::file_provider::revoke_prepared(prepared);
            }
        });
    }
}

#[cfg(all(debug_assertions, all(test, target_os = "macos")))]
mod tests {
    use super::*;
    use oink_foundation::content::OfferId;
    use oink_routing::dnd::{DragKey, PreparationKey};

    fn token(revision: u64) -> DndPreparation {
        DndPreparation::new(
            PreparationKey {
                drag: DragKey {
                    offer: OfferId {
                        clip: ClipId {
                            origin: DeviceId([1; 32]),
                            seq: 1,
                        },
                        generation: 1,
                    },
                    native_generation: 1,
                },
                metadata_revision: revision,
            },
            1,
        )
    }

    #[test]
    fn independent_ordinary_cancellation_preserves_grant_and_new_announcement() {
        let old = Slot::new(token(1), Vec::new(), None);
        let grant = DndTarget::new(
            old.token.clone(),
            oink_foundation::content::Holder {
                device: DeviceId([2; 32]),
                epoch: 3,
            },
            1,
            DeviceId([1; 32]),
        );
        let new = Slot::new(token(2), Vec::new(), None);
        old.cancellation.cancel();
        assert!(old.cancellation.is_cancelled());
        assert!(!new.cancellation.is_cancelled());
        assert!(old.token.current());
        assert!(grant.begin_current());
        grant.delivered();
        old.cancellation.cancel_and_wait();
        grant.leave();
        assert!(
            grant.content_current(),
            "ordinary cancellation cannot revoke a granted delivery"
        );
        assert!(!grant.begin_current());
        assert!(new.token.current());
        assert!(!new.cancellation.is_cancelled());
    }
}
