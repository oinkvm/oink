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
    v2::{
        Message, RawValue as V, dto,
        schema::{WireDto, WireValue},
    },
};
use std::{collections::BTreeMap, time::Duration};
const CAP: &str = "oink.layout/2";
const BUDGET: u64 = (MAX_BYTES + 1024 * 1024) as u64;
impl Node {
    fn receive_profile_message(
        &self,
        peer: oink_foundation::DeviceId,
        message: Message,
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
        let group = self
            .group_id()
            .ok_or_else(|| super::state::unavailable("Group unavailable"))?;
        match message {
            Message::LayoutProfilesState(message) => {
                #[cfg(test)]
                if self
                    .inner
                    .profiles
                    .defer_catalogs
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(ProfileError::new(ErrorCode::Busy, "Injected catalog delivery delay"));
                }
                let incoming = wire::read_catalog(message.catalog().map_err(wire::invalid)?)?;
                if message.group().map_err(wire::invalid)?.0 != group.0 || incoming.group.0 != group {
                    return Err(ProfileError::new(ErrorCode::NotMember, "Catalog group mismatch"));
                }
                let current = store::load(&self.inner.data_dir, group.into())?;
                let next = match &current {
                    Some(c) => c.receive(&incoming, self.id().into())?,
                    None => incoming.clone(),
                };
                let accepts_dependency = current.as_ref().is_none_or(|c| incoming.version >= c.version);
                let dependency = message.layout_version().map_err(wire::invalid)?;
                if accepts_dependency && let Some(version) = &dependency {
                    // Register the dependency before exposing the catalog: a
                    // display callback may arrive as soon as that save is visible.
                    self.link_config()
                        .map_err(super::state::unavailable)?
                        .observe_display_layout_dependency(group, version.clone())
                        .map_err(super::state::unavailable)?;
                }
                if current.as_ref() != Some(&next) {
                    self.persist_catalog(next)?;
                }
                if accepts_dependency {
                    lock(&self.inner.profiles.data).pending_layout = dependency;
                }
                // A target desktop's catalog arrived; merge the joining list in
                // the phone acceptor's place (H8).
                if let Ok(observation) = self.observe_profiles()
                    && let Some(merged) = self.substitute_merge(group.into(), &observation)?
                {
                    self.persist_catalog(merged)?;
                }
            }
            Message::LayoutProfilesJoin(message) => {
                #[cfg(test)]
                if self
                    .inner
                    .profiles
                    .defer_joins
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    self.inner
                        .profiles
                        .deferred_joins
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    return Err(ProfileError::new(ErrorCode::Busy, "Injected acceptor interruption"));
                }
                if message.group().map_err(wire::invalid)?.0 != group.0 {
                    return Err(ProfileError::new(ErrorCode::NotMember, "Join target mismatch"));
                }
                let id = message
                    .merge_id()
                    .map_err(wire::invalid)?
                    .0
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                let join = super::join::read(&self.inner.data_dir, &id)?
                    .ok_or_else(|| ProfileError::new(ErrorCode::NotMember, "Join not accepted locally"))?;
                let source = wire::read_catalog(message.catalog().map_err(wire::invalid)?)?;
                if join.acceptor.0 != self.id()
                    || join.initiator.0 != peer
                    || join.target.0 != group
                    || join.source != source.group
                {
                    return Err(ProfileError::new(ErrorCode::NotMember, "Join participant mismatch"));
                }
                let current = store::load(&self.inner.data_dir, group.into())?
                    .ok_or_else(|| super::state::unavailable("Accepting catalog unavailable"))?;
                // Admission must not depend on a live-facts query. Complete
                // saved snapshots are sufficient; a genuinely missing member
                // remains pending until an authoritative observation is available.
                let live =
                    self.observe_profiles()
                        .map(|o| o.snapshot)
                        .unwrap_or_else(|_| super::types::ArrangementSnapshot {
                            placements: Vec::new(),
                            native: Vec::new(),
                            disabled: Vec::new(),
                            screen_shared: Vec::new(),
                            topology: Default::default(),
                            active_profile_id: String::new(),
                        });
                let current_members = self.group_members().into_iter().map(crate::api::DeviceKey).collect();
                let context = super::composition::MergeContext {
                    source_members: &join.source_members,
                    target_members: &join.target_members,
                    current_members: &current_members,
                    live: &live,
                };
                let next = current.merge_group_with_context(&source, &id, self.id().into(), &context)?;
                if next != current {
                    self.persist_catalog(next)?;
                }
            }
            _ => {
                return Err(ProfileError::new(
                    ErrorCode::InvalidGeometry,
                    "Unexpected profile message",
                ));
            }
        }
        self.refresh_layout_profiles();
        Ok(())
    }
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
    let Ok(catalog) = wire::catalog(&catalog) else {
        return false;
    };
    let Ok(group) = catalog.group() else { return false };
    let Ok(message) =
        dto::LayoutProfilesState::from_raw([(1, group.write()), (2, catalog.write()), (4, version.write())].into())
    else {
        return false;
    };
    send(node, link, Message::LayoutProfilesState(message), group).await
}

async fn send(node: &Node, link: LinkId, message: Message, group: dto::Id16) -> bool {
    let Ok(links) = node.links() else {
        return false;
    };
    let options = oink_network::LowOptions {
        send_max: BUDGET,
        receive_max: 1024,
        operation_max: BUDGET + 1024,
    };
    let Ok(mut stream) = links.open_low(link, StreamKind::LayoutProfiles, options).await else {
        return false;
    };
    if stream.send(&message).await.is_err() {
        return false;
    }
    stream.finish();
    let Ok(Some(Message::LayoutProfilesReceipt(receipt))) = stream.recv().await else {
        return false;
    };
    if receipt.accepted() != Ok(true) || receipt.group() != Ok(group) {
        return false;
    }
    matches!(stream.recv().await, Ok(None))
}
async fn receive(node: Node, mut incoming: InboundLow) {
    let group = match &incoming.first {
        Message::LayoutProfilesState(m) => m.group(),
        Message::LayoutProfilesJoin(m) => m.group(),
        _ => return,
    };
    let Ok(group) = group else {
        return;
    };
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
    let accepted = tokio::task::spawn_blocking(move || {
        let result = node.receive_profile_message(peer, incoming.first, valid);
        if let Err(error) = &result {
            tracing::debug!(%peer, code=?error.code, reason=%error.message, "Profile transfer refused");
        }
        result
    })
    .await
    .is_ok_and(|v| v.is_ok());
    let Ok(receipt) = dto::LayoutProfilesReceipt::from_raw([(1, group.write()), (3, accepted.write())].into()) else {
        return;
    };
    if incoming
        .stream
        .send(&Message::LayoutProfilesReceipt(receipt))
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
                                if let Ok(joins) = node.pending_profile_joins(peer) {
                                    for join in joins {
                                        let Some(source) = join.catalog else {continue;};
                                        let Ok(catalog) = wire::catalog(&source) else {continue;};
                                        let group = oink_protocol::v2::schema::FixedBytes(join.target.0.0);
                                        let Ok(id) = (0..join.id.len()).step_by(2).map(|i|u8::from_str_radix(&join.id[i..i+2],16)).collect::<std::result::Result<Vec<_>,_>>() else {continue;};
                                        let Ok(message) = dto::LayoutProfilesJoin::from_raw([(1,group.write()),(2,V::Bytes(id)),(3,catalog.write())].into()) else {continue;};
                                        let _ = tokio::time::timeout(Duration::from_secs(30),send(&node,link,Message::LayoutProfilesJoin(message),group)).await;
                                    }
                                }
                                let catalog=lock(&node.inner.profiles.data).catalog.clone();
                                if let Some(catalog)=catalog && Some(catalog.group.0)==node.group_id() {
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
                    // Native tokens are refreshed by the platform display observer.
                    // Keep only logical recovery/retry work on this clock.
                    let _=tokio::task::spawn_blocking(move ||{node.poll_profile_operations();node.refresh_layout_profiles();}).await;
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
