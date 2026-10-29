//! Resolve a lazy native promise only after authenticating the requester. A
//! current clipboard's complete mapping is durable alongside its fulfilled bytes.
use super::*;

pub(super) struct Scope {
    pub roots: Vec<NodeId>,
    pub one_shot: Vec<NodeId>,
}
impl SourceGrants {
    pub(super) fn resolve_scope(
        &self,
        offered: Option<oink_foundation::content::OfferId>,
        source: &dyn Namespace,
        requested: &[NodeId],
        clipboard: bool,
    ) -> io::Result<Scope> {
        let (retained, native) = {
            let state = self.0.state.lock().unwrap();
            let current = state
                .journal
                .clipboard
                .as_ref()
                .filter(|current| clipboard && state.clipboard_confirmed && Some(current.offer) == offered);
            match current {
                Some(current) => (
                    current
                        .roots
                        .iter()
                        .map(|root| NodeId(root.snapshot.root))
                        .collect::<Vec<_>>(),
                    current.native.clone(),
                ),
                None => {
                    let mut roots = Vec::new();
                    let mut native = NativeFileMetadata::default();
                    for grant in state
                        .journal
                        .grants
                        .iter()
                        .filter(|grant| !clipboard && offered.is_some() && grant.offered == offered)
                    {
                        let live: Vec<_> = grant
                            .snapshots
                            .iter()
                            .filter(|s| !s.retired)
                            .map(|s| NodeId(s.root))
                            .collect();
                        native.extend(&grant.native.restricted(&live))?;
                        roots.extend(live);
                    }
                    roots.sort_unstable_by_key(|id| id.0);
                    roots.dedup();
                    (roots, native)
                }
            }
        };
        let initial = source.roots();
        let one_shot = source.one_shot_roots();
        let initial_native = source.native_file_metadata();
        // Reject unknown capabilities before asking a native owner to render.
        if requested.iter().any(|id| {
            !initial.iter().any(|root| root.id == *id)
                && !one_shot.contains(id)
                && !initial_native.promises.iter().any(|group| group.anchor == *id)
                && !retained.contains(id)
                && !native.promises.iter().any(|group| group.anchor == *id)
        }) {
            return Err(unavailable(Unavailable::Permission));
        }
        let needs_prepare = requested.iter().any(|id| {
            native.resolve_roots(&[*id], &retained).is_err()
                && (one_shot.contains(id) || initial_native.promises.iter().any(|group| group.anchor == *id))
        });
        if needs_prepare {
            source.prepare()?;
        }
        let mut mapping = source.native_file_metadata();
        // A restarted owner may have only the original promise capabilities.
        // Use the immutable mapping saved with the current slot's bytes.
        for group in native.promises {
            mapping.promises.retain(|existing| existing.anchor != group.anchor);
            mapping.promises.push(group);
        }
        let mut available = source.roots().iter().map(|root| root.id).collect::<Vec<_>>();
        available.extend(&retained);
        available.retain(|id| !mapping.promises.iter().any(|group| group.anchor == *id));
        available.sort_unstable_by_key(|id| id.0);
        available.dedup();
        let roots = mapping.resolve_roots(requested, &available)?;
        let mut one_shot = source.one_shot_roots().iter().try_fold(Vec::new(), |mut roots, id| {
            roots.extend(mapping.resolve_roots(&[*id], &available)?);
            Ok::<_, io::Error>(roots)
        })?;
        one_shot.extend(retained);
        one_shot.sort_unstable_by_key(|id| id.0);
        one_shot.dedup();
        Ok(Scope { roots, one_shot })
    }
}
