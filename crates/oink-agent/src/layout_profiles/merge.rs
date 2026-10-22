//! The accepting device merges each join once. Members otherwise use catalog LWW.
use super::{model::*, types::*};
use crate::api::DeviceKey;
use std::collections::BTreeSet;

impl Catalog {
    pub fn default_profile(&self) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.default_group == Some(self.group))
            .or_else(|| self.profiles.iter().find(|p| p.id == default_id(self.group)))
    }

    pub fn receive(&self, incoming: &Self, writer: DeviceKey) -> Result<Self> {
        incoming.validate()?;
        if incoming.group != self.group {
            return Err(ProfileError::new(
                ErrorCode::NotMember,
                "Catalog belongs to another group",
            ));
        }
        let local_only = self
            .completed_joins
            .keys()
            .any(|id| !incoming.completed_joins.contains_key(id));
        let remote_only = incoming
            .completed_joins
            .keys()
            .any(|id| !self.completed_joins.contains_key(id));
        if local_only && remote_only {
            // Concurrent accepting devices each completed a different join.
            let mut next = if incoming.version > self.version {
                incoming.clone()
            } else {
                self.clone()
            };
            next.union_profiles(self);
            next.union_profiles(incoming);
            for (id, completion) in self.completed_joins.iter().chain(&incoming.completed_joins) {
                if let Some(existing) = next.completed_joins.get(id) {
                    if existing != completion {
                        return Err(ProfileError::new(ErrorCode::StaleState, "Conflicting join completion"));
                    }
                } else {
                    next.completed_joins.insert(id.clone(), completion.clone());
                }
            }
            let latest = next
                .completed_joins
                .iter()
                .max_by(|(ai, a), (bi, b)| a.version.cmp(&b.version).then(ai.cmp(bi)))
                .map(|(_, v)| v)
                .expect("concurrent joins");
            let survivor = latest.default_profile.clone();
            let dropped = next
                .completed_joins
                .values()
                .flat_map(|j| j.dropped_defaults.iter().cloned())
                .collect();
            let fallback = next.active_profile_id.clone();
            next.finish_union(survivor.as_deref(), &dropped, &fallback);
            next.version.counter = self.version.counter.max(incoming.version.counter);
            next.advance(writer)?;
            next.disambiguate_names();
            next.validate()?;
            return Ok(next);
        }
        match incoming.version.cmp(&self.version) {
            std::cmp::Ordering::Greater => Ok(incoming.clone()),
            std::cmp::Ordering::Less => Ok(self.clone()),
            std::cmp::Ordering::Equal if incoming == self => Ok(self.clone()),
            _ => Err(ProfileError::new(
                ErrorCode::StaleState,
                "Same catalog version has different content",
            )),
        }
    }

    /// Only the named acceptor calls this, with the initiator's retained list.
    pub fn merge_group(&self, source: &Self, merge_id: &str, writer: DeviceKey) -> Result<Self> {
        source.validate()?;
        if self.completed_joins.contains_key(merge_id) {
            return Ok(self.clone());
        }
        let survivor = source.default_profile().map(|p| p.id.clone());
        let dropped = self
            .profiles
            .iter()
            .filter(|p| p.default_group == Some(self.group) || p.id == default_id(self.group))
            .filter(|p| Some(&p.id) != survivor.as_ref())
            .map(|p| p.id.clone())
            .collect::<BTreeSet<_>>();
        let mut next = self.clone();
        next.union_profiles(source);
        next.finish_union(survivor.as_deref(), &dropped, &source.active_profile_id);
        next.version.counter = next.version.counter.max(source.version.counter);
        next.advance(writer)?;
        next.completed_joins.insert(
            merge_id.into(),
            CompletedJoin {
                version: next.version.clone(),
                default_profile: survivor,
                dropped_defaults: dropped,
            },
        );
        next.disambiguate_names();
        next.validate()?;
        Ok(next)
    }

    fn union_profiles(&mut self, source: &Self) {
        for incoming in &source.profiles {
            if let Some(p) = self.profiles.iter_mut().find(|p| p.id == incoming.id) {
                let recency = p.last_used.clone().max(incoming.last_used.clone());
                if incoming.content_revision > p.content_revision {
                    *p = incoming.clone();
                }
                p.last_used = recency;
            } else {
                self.profiles.push(incoming.clone());
            }
        }
    }

    fn finish_union(&mut self, survivor: Option<&str>, dropped: &BTreeSet<String>, fallback: &str) {
        self.profiles
            .retain(|p| !dropped.contains(&p.id) || Some(p.id.as_str()) == survivor);
        for p in &mut self.profiles {
            p.default_group = (Some(p.id.as_str()) == survivor).then_some(self.group);
        }
        self.profiles
            .sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
        self.profiles.truncate(MAX_PROFILES);
        if !self.profiles.iter().any(|p| p.id == self.active_profile_id) {
            self.active_profile_id = self
                .profiles
                .iter()
                .find(|p| dropped.contains(&self.active_profile_id) && Some(p.id.as_str()) == survivor)
                .or_else(|| self.profiles.iter().find(|p| p.id == fallback))
                .or_else(|| self.profiles.first())
                .map(|p| p.id.clone())
                .unwrap_or_default();
        }
    }
}
