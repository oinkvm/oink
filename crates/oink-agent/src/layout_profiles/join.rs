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
    #[serde(default)]
    pub source_members: std::collections::BTreeSet<DeviceKey>,
    #[serde(default)]
    pub target_members: std::collections::BTreeSet<DeviceKey>,
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
/// The node is the sole writer of join bindings. Reload on its durable writes
/// and on group changes, not on each peer's two-second delivery retry.
#[derive(Debug, Default)]
pub(super) struct JoinCache {
    current: Option<(GroupKey, Vec<Join>)>,
}
impl JoinCache {
    fn load(&mut self, root: &Path, group: GroupKey) -> Result<&mut Vec<Join>> {
        if self.current.as_ref().is_none_or(|(cached, _)| *cached != group) {
            self.current = None;
            self.current = Some((group, list(root, group)?));
        }
        Ok(&mut self.current.as_mut().expect("loaded joins").1)
    }
    fn invalidate(&mut self) {
        self.current = None;
    }
}

pub(super) fn awaiting_catalog(root: &Path, group: GroupKey) -> Result<Option<Catalog>> {
    let mut group = group;
    let mut visited = std::collections::HashSet::new();
    while visited.insert(group) {
        let Some(join) = list(root, group)?.into_iter().find(|join| join.joining_member) else {
            return if visited.len() == 1 {
                Ok(None)
            } else {
                Err(failure("retained source catalog unavailable"))
            };
        };
        if let Some(catalog) = store::load(root, join.source)? {
            return Ok(Some(catalog));
        }
        // Membership may advance again before this observer receives the
        // intermediate catalog. Keep its last durable list read-only while
        // waiting for the newest group's authoritative catalog.
        group = join.source;
    }
    Err(failure("cyclic retained catalog handoff"))
}
impl Node {
    pub(crate) fn prepare_profile_join(&self, intent: &dto::MergeIntent, target_base: &dto::GroupState) -> Result<()> {
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
        // Only the source initiator transmits a saved list. A target or observer
        // can still be awaiting a previous join's catalog when this merge commits.
        // Requiring that catalog here would block projection and admission, which
        // are needed before the new members can exchange the missing catalog.
        // Persist their join binding without inventing or replacing a saved list.
        let catalog = if initiator.0 == self.id() {
            Some(store::load(&self.inner.data_dir, source)?.ok_or_else(|| failure("pre-join catalog unavailable"))?)
        } else {
            None
        };
        let mut cached = lock(&self.inner.profiles.joins);
        // A durable replace can fail after rename. Even that outcome must
        // retire an earlier empty scan before admission retries the binding.
        cached.invalidate();
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
                source_members: intent
                    .members()
                    .map_err(wire::invalid)?
                    .0
                    .iter()
                    .map(|m| {
                        oink_network::wire_identity::read_device(&m.device().map_err(wire::invalid)?)
                            .map(DeviceKey)
                            .map_err(wire::invalid)
                    })
                    .collect::<Result<_>>()?,
                target_members: oink_network::roster::Roster::verify(
                    target_base.roster().map_err(wire::invalid)?,
                    oink_network::roster::Anchor {
                        group: target.0,
                        root: target_base.roster().and_then(|r| r.root_id()).map_err(wire::invalid)?.0,
                    },
                )
                .map_err(wire::invalid)?
                .members()
                .map(|m| DeviceKey(m.device))
                .collect(),
            },
        )?;
        self.inner.profiles.wake.notify_waiters();
        Ok(())
    }

    pub(super) fn pending_profile_joins(&self, peer: oink_foundation::DeviceId) -> Result<Vec<Join>> {
        let group = self.group_id().ok_or_else(|| failure("group unavailable"))?.into();
        let current = lock(&self.inner.profiles.data).catalog.clone();
        let mut pending = Vec::new();
        let mut cached = lock(&self.inner.profiles.joins);
        for join in cached.load(&self.inner.data_dir, group)? {
            if join.initiator.0 != self.id() || join.acceptor.0 != peer || join.catalog.is_none() {
                continue;
            }
            if current
                .as_ref()
                .is_some_and(|c| c.group == group && c.completed_joins.contains_key(&join.id))
            {
                let mut completed = join.clone();
                completed.catalog = None;
                save(&self.inner.data_dir, &completed)?;
                *join = completed;
            } else {
                pending.push(join.clone());
            }
        }
        Ok(pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_join_retries_use_retained_bindings_until_invalidated() {
        let dir = tempfile::tempdir().unwrap();
        let group = GroupKey(oink_foundation::GroupId([2; 16]));
        let mut cache = JoinCache::default();
        assert!(cache.load(dir.path(), group).unwrap().is_empty());
        // Make a scan fail: stable retry must not reopen storage at all.
        std::fs::write(dir.path().join("layout-profile-joins"), b"not a directory").unwrap();
        for _ in 0..30 {
            assert!(cache.load(dir.path(), group).unwrap().is_empty());
        }
        cache.invalidate();
        assert!(cache.load(dir.path(), group).is_err());
        assert!(cache.current.is_none());
    }

    #[test]
    fn completed_older_join_bindings_do_not_block_listing_new_pending_joins() {
        let dir = tempfile::tempdir().unwrap();
        let id = "cd".repeat(32);
        let target = GroupKey(oink_foundation::GroupId([2; 16]));
        let old = Join {
            id: id.clone(),
            source: GroupKey(oink_foundation::GroupId([1; 16])),
            target,
            initiator: DeviceKey(oink_foundation::DeviceId([1; 32])),
            acceptor: DeviceKey(oink_foundation::DeviceId([2; 32])),
            joining_member: false,
            catalog: None,
            source_members: Default::default(),
            target_members: Default::default(),
        };
        let mut raw = serde_json::to_value(&old).unwrap();
        raw.as_object_mut().unwrap().remove("source_members");
        raw.as_object_mut().unwrap().remove("target_members");
        std::fs::create_dir_all(dir.path().join("layout-profile-joins")).unwrap();
        std::fs::write(path(dir.path(), &id), serde_json::to_vec(&raw).unwrap()).unwrap();
        let joins = list(dir.path(), target).unwrap();
        assert_eq!(joins.len(), 1);
        assert!(joins[0].catalog.is_none());
        assert!(joins[0].source_members.is_empty());
        assert!(joins[0].target_members.is_empty());
    }
}
