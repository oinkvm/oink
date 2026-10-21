//! Capability-gated full-catalog Low synchronization. Receiving a catalog never
//! applies a profile, changes visibility, or invokes a platform writer.
use super::{
    model::{Catalog, MAX_BYTES},
    store,
    types::*,
    wire,
};
use crate::node::{Node, lock};
use oink_network::link::{InboundLow, LinkId, ModuleEvent, ModulePort};
use oink_protocol::{
    frame::StreamKind,
    v1::{
        Message, RawValue as V, dto,
        schema::{WireDto, WireValue},
    },
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};
const CAP: &str = "oink.layout.profiles/2";
const BUDGET: u64 = (MAX_BYTES + 1024 * 1024) as u64;
fn digest(c: &dto::LayoutProfileCatalogV2) -> oink_protocol::v1::Result<[u8; 32]> {
    Ok(Sha256::digest(c.encode(MAX_BYTES)?).into())
}
impl Node {
    pub(super) fn reconcile_profile_merges(&self) -> Result<()> {
        let _gate = self
            .inner
            .profiles
            .gate
            .try_lock()
            .map_err(|_| ProfileError::new(ErrorCode::Busy, "Profile operation in progress"))?;
        self.profile_snapshot()?;
        let proofs = self
            .link_config()
            .map_err(super::state::unavailable)?
            .layout_profile_merges()
            .map_err(super::state::unavailable)?;
        let mut pending = false;
        for proof in proofs {
            let intent = proof.certificate().and_then(|c| c.intent()).map_err(wire::invalid)?;
            let source =
                oink_network::group_merge::anchor(&intent.source().map_err(wire::invalid)?).map_err(wire::invalid)?;
            let id = intent
                .merge_id()
                .map_err(wire::invalid)?
                .0
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let catalog = lock(&self.inner.profiles.data).catalog.clone().expect("catalog");
            if catalog.imported_merges.contains(&id) {
                continue;
            }
            if let Some(source) = store::load(&self.inner.data_dir, source.group.into())? {
                self.persist_catalog(catalog.merge_group(&source, &id, self.id().into())?)?;
            } else if !predates_profile_store(&proof, self.initialize_profile_store()?)? {
                let config = self.link_config().map_err(super::state::unavailable)?;
                let members = intent.members().map_err(wire::invalid)?.0;
                // No import receipt is invented. If every source member is
                // presently legacy, keep local editing usable; a later capable
                // source still imports its archive through the certified path.
                pending |= import_may_arrive(members.iter().map(|m| {
                    let device = oink_network::wire_identity::read_device(&m.device().ok()?).ok()?;
                    if device == self.id() {
                        return Some(true);
                    }
                    config.peer_agreement(device).map(|a| a.contains(CAP))
                }));
            }
        }
        lock(&self.inner.profiles.data).pending_import = pending;
        Ok(())
    }
    fn receive_profile_catalog(
        &self,
        peer: oink_foundation::DeviceId,
        message: dto::LayoutProfilesStateV2,
        valid: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<()> {
        let _gate = self
            .inner
            .profiles
            .gate
            .lock()
            .map_err(|_| ProfileError::new(ErrorCode::Busy, "Profile owner unavailable"))?;
        if !valid() || !self.group_members().contains(&peer) {
            return Err(ProfileError::new(
                ErrorCode::NotMember,
                "Catalog sender is no longer a member",
            ));
        }
        self.profile_snapshot()?;
        let incoming = wire::read_catalog(message.catalog().map_err(wire::invalid)?)?;
        if message.group().map_err(wire::invalid)?.0 != incoming.group.0.0 {
            return Err(ProfileError::new(
                ErrorCode::NotMember,
                "Catalog envelope group mismatch",
            ));
        }
        let current = lock(&self.inner.profiles.data).catalog.clone().expect("catalog");
        let next = if incoming.group == current.group {
            current.receive(&incoming, self.id().into())?
        } else {
            let proof = message
                .merge()
                .map_err(wire::invalid)?
                .ok_or_else(|| ProfileError::new(ErrorCode::NotMember, "Missing merge proof"))?;
            let known = self
                .link_config()
                .map_err(super::state::unavailable)?
                .layout_profile_merges()
                .map_err(super::state::unavailable)?;
            if !known.contains(&proof) {
                return Err(ProfileError::new(
                    ErrorCode::NotMember,
                    "Merge proof has not been committed locally",
                ));
            }
            let intent = proof.certificate().and_then(|c| c.intent()).map_err(wire::invalid)?;
            if intent.source().and_then(|s| s.group()).map_err(wire::invalid)?.0 != incoming.group.0.0 {
                return Err(ProfileError::new(ErrorCode::NotMember, "Merge source mismatch"));
            }
            let id = intent
                .merge_id()
                .map_err(wire::invalid)?
                .0
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            current.merge_group(&incoming, &id, self.id().into())?
        };
        if next != current {
            self.persist_catalog(next)?;
        }
        if incoming.group == current.group && incoming.version >= current.version {
            lock(&self.inner.profiles.data).pending_layout = message.layout_version().map_err(wire::invalid)?;
        }
        self.refresh_layout_profiles();
        Ok(())
    }
}

fn predates_profile_store(proof: &dto::MergeTransferBundle, activated_ms: u64) -> Result<bool> {
    // The frozen merge protocol has no profile capability bit. Its certified
    // plan does contain issuance times for the imported membership records.
    // With no archived source catalog, only a plan strictly older than local
    // feature activation is historical. Unknown times remain pending. An
    // archive arriving later is still imported; this is not an import receipt.
    let adds = proof
        .plan()
        .and_then(|plan| plan.unsigned_adds())
        .map_err(wire::invalid)?
        .0;
    if adds.is_empty() {
        return Ok(false);
    }
    for add in adds {
        let issued_ms = add.issued_ms().map_err(wire::invalid)?;
        if issued_ms == 0 || issued_ms >= activated_ms {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn deliver(node: &Node, link: LinkId, catalog: Catalog) -> bool {
    if lock(&node.inner.profiles.data)
        .operations
        .back()
        .is_some_and(|op| op.status == OperationStatus::Running)
    {
        return false;
    }
    let Ok(config) = node.link_config() else { return false };
    let version = config.applied().version.clone();
    let Ok(links) = node.links() else { return false };
    let Ok(catalog) = wire::catalog(&catalog) else {
        return false;
    };
    let Ok(digest) = digest(&catalog) else { return false };
    let Ok(group) = catalog.group() else { return false };
    let Ok(message) =
        dto::LayoutProfilesStateV2::from_raw([(1, group.write()), (2, catalog.write()), (4, version.write())].into())
    else {
        return false;
    };
    let options = oink_network::LowOptions {
        send_max: BUDGET,
        receive_max: 1024,
        operation_max: BUDGET + 1024,
    };
    let Ok(mut stream) = links.open_low(link, StreamKind::LayoutProfilesV2, options).await else {
        return false;
    };
    if stream.send(&Message::LayoutProfilesStateV2(message)).await.is_err() {
        return false;
    }
    stream.finish();
    let Ok(Some(Message::LayoutProfilesReceiptV2(receipt))) = stream.recv().await else {
        return false;
    };
    if receipt.accepted() != Ok(true) || receipt.group() != Ok(group) || receipt.digest().map(|h| h.0) != Ok(digest) {
        return false;
    }
    matches!(stream.recv().await, Ok(None))
}
async fn receive(node: Node, mut incoming: InboundLow) {
    let Message::LayoutProfilesStateV2(message) = incoming.first else {
        return;
    };
    let Ok(catalog) = message.catalog() else { return };
    let Ok(hash) = digest(&catalog) else { return };
    let Ok(group) = message.group() else { return };
    let peer = incoming.stream.peer();
    if !node
        .link_config()
        .ok()
        .and_then(|c| c.peer_agreement(peer))
        .is_some_and(|a| a.contains(CAP))
    {
        return;
    }
    if !matches!(incoming.stream.recv().await, Ok(None)) {
        return;
    }
    let valid = incoming.stream.effect_validity();
    let accepted = tokio::task::spawn_blocking(move || node.receive_profile_catalog(peer, message, valid))
        .await
        .is_ok_and(|v| v.is_ok());
    let Ok(receipt) = dto::LayoutProfilesReceipt::from_raw(
        [(1, group.write()), (2, V::Bytes(hash.to_vec())), (3, accepted.write())].into(),
    ) else {
        return;
    };
    if incoming
        .stream
        .send(&Message::LayoutProfilesReceiptV2(receipt))
        .await
        .is_ok()
    {
        incoming.stream.finish();
    }
}
pub(crate) fn start(node: Node, mut port: ModulePort) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut peers = BTreeMap::<oink_foundation::DeviceId, (LinkId, tokio::task::AbortHandle)>::new();
        // JoinSet aborts owned delivery tasks even when the parent is cancelled.
        let mut outgoing = tokio::task::JoinSet::new();
        let mut incoming = tokio::task::JoinSet::new();
        let mut clock = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                event=port.next()=>match event {
                    Some(ModuleEvent::LinkReady{link,peer})=>{
                        if let Some((_,old))=peers.remove(&peer){old.abort();}
                        if !node.link_config().ok().and_then(|c|c.peer_agreement(peer)).is_some_and(|a|a.contains(CAP)){continue}
                        let node=node.clone();
                        let task=outgoing.spawn(async move {
                            let mut delivered=None;let mut backoff=Duration::from_secs(2);
                            loop {
                                let catalog=lock(&node.inner.profiles.data).catalog.clone();
                                if let Some(catalog)=catalog {
                                    let version=(catalog.group,catalog.version.clone());
                                    if delivered.as_ref()!=Some(&version) {
                                        if tokio::time::timeout(Duration::from_secs(30),deliver(&node,link,catalog)).await.unwrap_or(false){delivered=Some(version);backoff=Duration::from_secs(2);}else{tokio::time::sleep(backoff+Duration::from_millis(u64::from(peer.0[0])*3)).await;backoff=(backoff*2).min(Duration::from_secs(60));continue;}
                                    }
                                }
                                tokio::select!{_=node.inner.profiles.wake.notified()=>{},_=tokio::time::sleep(Duration::from_secs(2))=>{}}
                            }
                        });peers.insert(peer,(link,task));
                    }
                    Some(ModuleEvent::LinkLost{link,peer,..})=>{if peers.get(&peer).is_some_and(|(old,_)|*old==link) && let Some((_,task))=peers.remove(&peer){task.abort();}}
                    Some(ModuleEvent::InboundLow(operation))=>{let node=node.clone();incoming.spawn(async move{let _=tokio::time::timeout(Duration::from_secs(30),receive(node,*operation)).await;});}
                    None=>break,
                    _=>{}
                },
                _=clock.tick()=>{
                    let node=node.clone();
                    // Also refresh read-only native tokens after a display/mode
                    // change; unsupported test backends simply have no token.
                    let _=tokio::task::spawn_blocking(move ||{let _=node.refresh_native_arrangement();let _=node.reconcile_profile_merges();node.poll_profile_operations();node.refresh_layout_profiles();}).await;
                }
                _=incoming.join_next(),if !incoming.is_empty()=>{}
                _=outgoing.join_next(),if !outgoing.is_empty()=>{}
            }
        }
        for (_, (_, task)) in peers {
            task.abort();
        }
        incoming.abort_all();
    })
}

fn import_may_arrive(capabilities: impl Iterator<Item = Option<bool>>) -> bool {
    let capabilities = capabilities.collect::<Vec<_>>();
    capabilities.is_empty() || capabilities.iter().any(|c| *c != Some(false))
}
#[cfg(test)]
mod tests {
    use super::import_may_arrive;
    #[test]
    fn legacy_source_does_not_hold_catalog_controls_pending() {
        assert!(!import_may_arrive([Some(false), Some(false)].into_iter()));
        assert!(import_may_arrive([Some(false), None].into_iter()));
        assert!(import_may_arrive([Some(false), Some(true)].into_iter()));
        assert!(import_may_arrive([].into_iter()));
    }
}
