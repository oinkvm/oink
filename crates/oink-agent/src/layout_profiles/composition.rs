//! Pure, roster-scoped composition. Membership direction is independent of profile policy.
use super::{model::*, types::*};
use crate::api::DeviceKey;
use oink_foundation::{Point, Rect};
use std::collections::BTreeSet;

#[derive(Debug)]
pub(super) struct MergeContext<'a> {
    pub source_members: &'a BTreeSet<DeviceKey>,
    pub target_members: &'a BTreeSet<DeviceKey>,
    pub current_members: &'a BTreeSet<DeviceKey>,
    pub live: &'a ArrangementSnapshot,
}
fn scoped(p: &Profile, members: &BTreeSet<DeviceKey>) -> Profile {
    let mut p = p.clone();
    p.content.placements.retain(|v| members.contains(&v.device));
    p.content.native.retain(|v| members.contains(&v.device));
    p.content.disabled.retain(|v| members.contains(&v.screen.device));
    p.content.screen_shared.retain(|v| members.contains(&v.device));
    p.match_state.online_devices.clear();
    p.match_state.connected_screens.retain(|d, _| members.contains(d));
    if let Some(t) = &mut p.saved_topology {
        t.retain(|d| members.contains(&d.device));
    }
    p
}
fn supplement(p: &mut Profile, fallback: &ArrangementSnapshot, members: &BTreeSet<DeviceKey>) {
    for incoming in fallback.placements.iter().filter(|v| members.contains(&v.device)) {
        if !p.content.placements.iter().any(|v| v.device == incoming.device) {
            p.content.placements.push(incoming.clone());
            if let Some(n) = fallback.native.iter().find(|v| v.device == incoming.device) {
                p.content.native.push(n.clone());
            }
            p.content.screen_shared.push(DeviceSharing {
                device: incoming.device,
                shared: screen_shared(fallback, incoming.device),
            });
        } else if let (Some(n), Some(other)) = (
            p.content.native.iter_mut().find(|n| n.device == incoming.device),
            fallback.native.iter().find(|n| n.device == incoming.device),
        ) {
            for s in &other.screens {
                if !n.screens.iter().any(|v| v.screen == s.screen) {
                    n.screens.push(s.clone());
                }
            }
        }
    }
    for bit in &fallback.disabled {
        if members.contains(&bit.screen.device) && !p.content.disabled.iter().any(|d| d.screen == bit.screen) {
            p.content.disabled.push(bit.clone());
        }
    }
    for n in &p.content.native {
        p.match_state
            .connected_screens
            .entry(n.device)
            .or_insert_with(|| Some(n.screens.iter().map(|s| s.screen.display.clone()).collect()));
    }
    for n in &mut p.content.native {
        n.token.clear();
    }
    canonicalize(&mut p.content);
}
fn active(c: &Catalog, members: &BTreeSet<DeviceKey>, live: &ArrangementSnapshot) -> Result<Profile> {
    let mut p = scoped(c.active(), members);
    supplement(&mut p, live, members);
    // Selection-only changes (including deleting the active profile) do not
    // load it. A stale selected snapshot must not manufacture visibility on
    // another group's profiles; known owner bits describe the actual state.
    for bit in &mut p.content.disabled {
        if let Some(current) = live.disabled.iter().find(|d| d.screen == bit.screen) {
            bit.disabled = current.disabled;
        }
    }
    for current in live.placements.iter().filter(|p| members.contains(&p.device)) {
        set_screen_shared(&mut p.content, current.device, screen_shared(live, current.device));
    }
    canonicalize(&mut p.content);
    if members
        .iter()
        .any(|d| !p.content.placements.iter().any(|p| p.device == *d))
    {
        return Err(ProfileError::new(
            ErrorCode::FactsUnavailable,
            "Joining member geometry unavailable",
        ));
    }
    validate_snapshot(&p.content)?;
    Ok(p)
}
fn bounds(s: &ArrangementSnapshot) -> Option<(f64, f64, f64)> {
    let mut b: Option<(f64, f64, f64)> = None;
    for p in &s.placements {
        if let Some(n) = s.native.iter().find(|n| n.device == p.device) {
            for s in &n.screens {
                let r = Rect::new(
                    p.origin.x + s.origin.x * p.scale,
                    p.origin.y + s.origin.y * p.scale,
                    s.size.w * p.scale,
                    s.size.h * p.scale,
                );
                b = Some(b.map_or((r.x, r.y, r.right()), |(x, y, right)| {
                    (x.min(r.x), y.min(r.y), right.max(r.right()))
                }));
            }
        }
    }
    b
}
fn append(p: &mut Profile, template: &Profile) -> Result<bool> {
    let missing = template
        .content
        .placements
        .iter()
        .filter(|v| !p.content.placements.iter().any(|p| p.device == v.device))
        .map(|p| p.device)
        .collect::<BTreeSet<_>>();
    if missing.is_empty() {
        return Ok(false);
    }
    let incoming = scoped(template, &missing);
    let shift = match (bounds(&p.content), bounds(&incoming.content)) {
        (Some((_, top, right)), Some((left, y, _))) => Point::new(
            right + (10.0 / 686.0_f64).max(p.content.topology.portal_snap) - left,
            top - y,
        ),
        (None, Some((left, top, _))) => Point::new(-left, -top),
        _ => Point::default(),
    };
    for mut v in incoming.content.placements {
        v.origin.x += shift.x;
        v.origin.y += shift.y;
        p.content.placements.push(v);
    }
    p.content.native.extend(incoming.content.native);
    p.content.disabled.extend(incoming.content.disabled);
    p.content.screen_shared.extend(incoming.content.screen_shared);
    p.match_state
        .connected_screens
        .extend(incoming.match_state.connected_screens);
    p.saved_topology = match (p.saved_topology.take(), incoming.saved_topology) {
        (Some(mut a), Some(b)) => {
            a.extend(b);
            a.sort_by_key(|d| d.device);
            Some(a)
        }
        _ => None,
    };
    canonicalize(&mut p.content);
    validate_snapshot(&p.content)?;
    Ok(true)
}
impl Catalog {
    pub(super) fn merge_group_with_context(
        &self,
        source: &Self,
        id: &str,
        writer: DeviceKey,
        ctx: &MergeContext<'_>,
    ) -> Result<Self> {
        source.validate()?;
        if self.completed_joins.contains_key(id) {
            return Ok(self.clone());
        }
        if ctx.source_members.is_empty() || ctx.target_members.is_empty() {
            return Err(ProfileError::new(
                ErrorCode::InvalidGeometry,
                "Missing original group roster",
            ));
        }
        // A later membership commit/profile handoff can finish first. Preserve
        // those already admitted devices while the original roster sizes still
        // choose the policy for this particular committed join.
        let mut target_members = ctx.target_members.clone();
        target_members.extend(
            ctx.current_members
                .iter()
                .filter(|d| !ctx.source_members.contains(d))
                .copied(),
        );
        let source_active = active(source, ctx.source_members, ctx.live)?;
        let target_active = active(self, &target_members, ctx.live)?;
        // A singleton contributes its computer, not its profiles. When both
        // original groups are singletons, keep the acceptor's catalog.
        let keep_one_catalog = ctx.source_members.len() == 1 || ctx.target_members.len() == 1;
        let mut next = self.clone();
        next.version.counter = next.version.counter.max(source.version.counter);
        next.advance(writer)?;
        let mut appends = Vec::new();
        let mut dropped = BTreeSet::new();
        let survivor;
        if keep_one_catalog {
            let (keep, own, own_active, incoming) = if ctx.source_members.len() > 1 {
                (source, ctx.source_members, &source_active, &target_active)
            } else {
                (self, &target_members, &target_active, &source_active)
            };
            survivor = keep.default_profile().map(|p| p.id.clone());
            next.extensions = keep.extensions.clone();
            next.profiles = keep.profiles.iter().map(|p| scoped(p, own)).collect();
            next.active_profile_id = keep.active_profile_id.clone();
            for p in &mut next.profiles {
                let before = p.clone();
                supplement(p, &own_active.content, own);
                append(p, incoming)?;
                if p.content != before.content || p.saved_topology != before.saved_topology {
                    p.content_revision = next.version.clone();
                }
            }
            appends.push(incoming.clone());
        } else {
            survivor = source.default_profile().map(|p| p.id.clone());
            dropped = self
                .profiles
                .iter()
                .filter(|p| p.default_group == Some(self.group) || p.id == default_id(self.group))
                .filter(|p| Some(&p.id) != survivor.as_ref())
                .map(|p| p.id.clone())
                .collect();
            next.profiles.clear();
            for (catalog, own, own_active, incoming, is_source) in [
                (self, &target_members, &target_active, &source_active, false),
                (source, ctx.source_members, &source_active, &target_active, true),
            ] {
                for old in &catalog.profiles {
                    let mut p = scoped(old, own);
                    if !is_source && dropped.contains(&p.id) {
                        p.id = hash(&("oink.preserved-default-layout/1", &old.id))[..32].into();
                        p.default_group = None;
                        if p.name == ProfileName::Default {
                            p.name = ProfileName::Custom {
                                value: format!("Default layout {}", &p.id[..8]),
                            };
                        }
                    }
                    supplement(&mut p, &own_active.content, own);
                    append(&mut p, incoming)?;
                    if p.content != old.content || p.saved_topology != old.saved_topology {
                        p.content_revision = next.version.clone();
                    }
                    next.union_profile(&p);
                }
            }
            appends.push(source_active);
            appends.push(target_active);
        }
        next.finish_union(survivor.as_deref(), &dropped, &source.active_profile_id);
        next.completed_joins.insert(
            id.into(),
            CompletedJoin {
                version: next.version.clone(),
                default_profile: survivor,
                dropped_defaults: dropped,
                appends,
            },
        );
        next.disambiguate_names();
        next.validate()?;
        Ok(next)
    }
    pub(super) fn complete_concurrent_appends(&mut self) -> Result<()> {
        let records = self
            .completed_joins
            .values()
            .flat_map(|j| j.appends.iter())
            .cloned()
            .collect::<Vec<_>>();
        for a in records {
            for p in &mut self.profiles {
                if append(p, &a)? {
                    p.content_revision = self.version.clone();
                }
            }
        }
        Ok(())
    }
}
