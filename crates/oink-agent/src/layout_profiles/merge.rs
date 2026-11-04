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
            next.complete_concurrent_appends()?;
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

    /// Synthetic catalog fixtures supply their roster via match_state. Production
    /// must use the signed pre-merge membership context, including empty profiles.
    #[cfg(test)]
    pub fn merge_group(&self, source: &Self, id: &str, writer: DeviceKey) -> Result<Self> {
        let members = |c: &Self| {
            let mut ids = c
                .active()
                .match_state
                .connected_screens
                .keys()
                .copied()
                .collect::<BTreeSet<_>>();
            if ids.is_empty() {
                ids.insert(c.version.writer);
            }
            ids
        };
        let source_members = members(source);
        let target_members = members(self);
        let mut live = self.active().content.clone();
        for p in &source.active().content.placements {
            if !live.placements.iter().any(|v| v.device == p.device) {
                live.placements.push(p.clone());
                live.native.extend(
                    source
                        .active()
                        .content
                        .native
                        .iter()
                        .filter(|n| n.device == p.device)
                        .cloned(),
                );
                live.disabled.extend(
                    source
                        .active()
                        .content
                        .disabled
                        .iter()
                        .filter(|d| d.screen.device == p.device)
                        .cloned(),
                );
                live.screen_shared.extend(
                    source
                        .active()
                        .content
                        .screen_shared
                        .iter()
                        .filter(|d| d.device == p.device)
                        .cloned(),
                );
            }
        }
        self.merge_group_with_context(
            source,
            id,
            writer,
            &super::composition::MergeContext {
                source_members: &source_members,
                target_members: &target_members,
                current_members: &source_members.union(&target_members).copied().collect(),
                live: &live,
            },
        )
    }

    fn union_profiles(&mut self, source: &Self) {
        for incoming in &source.profiles {
            self.union_profile(incoming);
        }
    }

    pub(super) fn union_profile(&mut self, incoming: &Profile) {
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

    pub(super) fn finish_union(&mut self, survivor: Option<&str>, dropped: &BTreeSet<String>, fallback: &str) {
        self.profiles
            .retain(|p| !dropped.contains(&p.id) || Some(p.id.as_str()) == survivor);
        for p in &mut self.profiles {
            p.default_group = (Some(p.id.as_str()) == survivor).then_some(self.group);
        }
        self.profiles
            .sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
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
        // Resolve selection before LRU eviction: a replaced active default
        // selects the surviving default. Neither that default nor the final
        // active profile may be evicted, even if its recency is oldest.
        let protected = self
            .profiles
            .iter()
            .filter(|p| p.default_group == Some(self.group) || p.id == self.active_profile_id)
            .count();
        let mut ordinary_slots = MAX_PROFILES.saturating_sub(protected);
        self.profiles.retain(|p| {
            if p.default_group == Some(self.group) || p.id == self.active_profile_id {
                true
            } else if ordinary_slots > 0 {
                ordinary_slots -= 1;
                true
            } else {
                false
            }
        });
    }
}
