//! Durable handoff at membership projection. Only the initiator retains a list
//! to send; only the acceptor records authority to consume that particular list.
use super::{
    model::{Catalog, MAX_BYTES},
    store,
    types::*,
    wire,
};
use crate::{
    api::{DeviceKey, GroupKey},
    node::{Node, lock},
};
use oink_protocol::v1::dto;
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Join {
    pub id: String,
    pub source: GroupKey,
    pub target: GroupKey,
    pub initiator: DeviceKey,
    pub acceptor: DeviceKey,
    pub joining_member: bool,
    pub catalog: Option<Catalog>,
}
fn failure(e: impl std::fmt::Display) -> ProfileError {
    ProfileError::new(ErrorCode::PersistenceFailed, format!("Profile join: {e}"))
}
fn path(root: &Path, id: &str) -> std::path::PathBuf {
    root.join("layout-profile-joins").join(format!("{id}.json"))
}
pub(super) fn read(root: &Path, id: &str) -> Result<Option<Join>> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failure("invalid join ID"));
    }
    let file = match std::fs::File::open(path(root, id)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failure(e)),
    };
    let mut bytes = Vec::new();
    file.take((MAX_BYTES * 2 + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(failure)?;
    if bytes.len() > MAX_BYTES * 2 {
        return Err(failure("join size limit"));
    }
    let join: Join = serde_json::from_slice(&bytes).map_err(failure)?;
    if join.id != id {
        return Err(failure("join identity mismatch"));
    }
    if let Some(c) = &join.catalog {
        c.validate()?;
    }
    Ok(Some(join))
}
fn save(root: &Path, join: &Join) -> Result<()> {
    let p = path(root, &join.id);
    std::fs::create_dir_all(p.parent().expect("join directory")).map_err(failure)?;
    let bytes = serde_json::to_vec(join).map_err(failure)?;
    if bytes.len() > MAX_BYTES * 2 {
        return Err(failure("join size limit"));
    }
    oink_foundation::storage::atomic_write(&p, &bytes).map_err(failure)
}
pub(super) fn list(root: &Path, group: GroupKey) -> Result<Vec<Join>> {
    let dir = match std::fs::read_dir(root.join("layout-profile-joins")) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(failure(e)),
    };
    let mut out = Vec::new();
    for entry in dir {
        let entry = entry.map_err(failure)?;
        if entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        let id = entry
            .path()
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        if let Some(join) = read(root, &id)?
            && join.target == group
        {
            out.push(join);
        }
    }
    Ok(out)
}
pub(super) fn awaiting_catalog(root: &Path, group: GroupKey) -> Result<Option<Catalog>> {
    for join in list(root, group)? {
        if join.joining_member {
            return store::load(root, join.source)?
                .map(Some)
                .ok_or_else(|| failure("retained source catalog unavailable"));
        }
    }
    Ok(None)
}
impl Node {
    pub(crate) fn prepare_profile_join(&self, intent: &dto::MergeIntent) -> Result<()> {
        let id = intent
            .merge_id()
            .map_err(wire::invalid)?
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if read(&self.inner.data_dir, &id)?.is_some() {
            return Ok(());
        }
        let source = GroupKey(oink_foundation::GroupId(
            intent.source().and_then(|v| v.group()).map_err(wire::invalid)?.0,
        ));
        let target = GroupKey(oink_foundation::GroupId(
            intent.target().and_then(|v| v.group()).map_err(wire::invalid)?.0,
        ));
        let initiator = DeviceKey(
            oink_network::wire_identity::read_device(&intent.source_issuer().map_err(wire::invalid)?)
                .map_err(wire::invalid)?,
        );
        let acceptor = DeviceKey(
            oink_network::wire_identity::read_device(&intent.target_issuer().map_err(wire::invalid)?)
                .map_err(wire::invalid)?,
        );
        let joining_member = intent.members().map_err(wire::invalid)?.0.iter().any(|m| {
            m.device()
                .ok()
                .and_then(|d| oink_network::wire_identity::read_device(&d).ok())
                == Some(self.id())
        });
        if !joining_member && acceptor.0 != self.id() {
            return Ok(());
        }
        let _gate = self
            .inner
            .profiles
            .gate
            .try_lock()
            .map_err(|_| ProfileError::new(ErrorCode::Busy, "Profile operation in progress"))?;
        let expected = if joining_member { source } else { target };
        // Joining transfers the already-saved list, not a live arrangement.
        // Owner facts can be missing until admission completes; observing them
        // here would defer projection and the queued commit/admission messages
        // needed to make those facts available. The profile gate fences saves,
        // so read the durable list directly, including during startup recovery.
        let current =
            store::load(&self.inner.data_dir, expected)?.ok_or_else(|| failure("pre-join catalog unavailable"))?;
        let catalog = (initiator.0 == self.id()).then_some(current);
        save(
            &self.inner.data_dir,
            &Join {
                id,
                source,
                target,
                initiator,
                acceptor,
                joining_member,
                catalog,
            },
        )
    }

    pub(super) fn pending_profile_joins(&self, peer: oink_foundation::DeviceId) -> Result<Vec<Join>> {
        let group = self.group_id().ok_or_else(|| failure("group unavailable"))?.into();
        let current = lock(&self.inner.profiles.data).catalog.clone();
        let mut pending = Vec::new();
        for mut join in list(&self.inner.data_dir, group)? {
            if join.initiator.0 != self.id() || join.acceptor.0 != peer || join.catalog.is_none() {
                continue;
            }
            if current
                .as_ref()
                .is_some_and(|c| c.group == group && c.completed_joins.contains_key(&join.id))
            {
                join.catalog = None;
                save(&self.inner.data_dir, &join)?;
            } else {
                pending.push(join);
            }
        }
        Ok(pending)
    }
}
