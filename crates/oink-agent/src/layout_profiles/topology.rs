//! Physical desktop identity, deliberately independent of presence and layout placement.
use super::{model, types::*};
use crate::{
    api::DeviceKey,
    node::{Node, lock},
};
use oink_protocol::v1::dto;
use serde::{Deserialize, Serialize};

pub const CAP: &str = "oink.layout.topology/1";
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhysicalScreen {
    pub id: String,
    pub bounds: oink_foundation::Rect,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub scale: f64,
    pub rotation: u16,
    pub primary: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhysicalDevice {
    pub device: DeviceKey,
    pub screens: Vec<PhysicalScreen>,
}
pub type PhysicalTopology = Vec<PhysicalDevice>;

pub fn from_facts(table: &crate::config_module::MemberTable) -> Option<PhysicalTopology> {
    let mut topology = Vec::new();
    for member in &table.members {
        let mut screens = Vec::new();
        for display in member.facts.as_ref()?.displays().ok()?.0 {
            let bounds = display.bounds().ok()?;
            screens.push(PhysicalScreen {
                id: display.key().ok()?.0,
                bounds: oink_foundation::Rect::new(
                    bounds.x().ok()?,
                    bounds.y().ok()?,
                    bounds.w().ok()?,
                    bounds.h().ok()?,
                ),
                pixel_width: display.pixel_width().ok()?,
                pixel_height: display.pixel_height().ok()?,
                scale: display.scale().ok()?,
                rotation: display.rotation_deg().ok()?,
                primary: display.is_primary().ok()?,
            });
        }
        screens.sort_by(|a, b| a.id.cmp(&b.id));
        topology.push(PhysicalDevice {
            device: member.device.into(),
            screens,
        });
    }
    topology.sort_by_key(|d| d.device);
    Some(topology)
}

pub fn matches_native(topology: &PhysicalTopology, snapshot: &ArrangementSnapshot) -> bool {
    topology.iter().all(|d| {
        if d.screens.is_empty() {
            return !snapshot.native.iter().any(|n| n.device == d.device);
        }
        snapshot.native.iter().find(|n| n.device == d.device).is_some_and(|n| {
            n.screens.len() == d.screens.len()
                && d.screens.iter().all(|s| {
                    (!s.primary || n.primary.display == s.id)
                        && n.screens.iter().any(|t| {
                            t.screen.display == s.id
                                && t.origin.x == s.bounds.x
                                && t.origin.y == s.bounds.y
                                && t.size.w == s.bounds.w
                                && t.size.h == s.bounds.h
                        })
                })
        })
    })
}

pub fn validate(topology: &PhysicalTopology) -> Result<()> {
    let invalid = || ProfileError::new(ErrorCode::InvalidGeometry, "Invalid physical topology");
    if topology.len() > 16 {
        return Err(invalid());
    }
    let mut devices = std::collections::BTreeSet::new();
    let mut count = 0;
    for device in topology {
        if !devices.insert(device.device) {
            return Err(invalid());
        }
        let mut screens = std::collections::BTreeSet::new();
        for screen in &device.screens {
            count += 1;
            let r = screen.bounds;
            if count > 256
                || !screens.insert(&screen.id)
                || screen.id.is_empty()
                || screen.id.len() > 512
                || ![r.x, r.y, r.w, r.h, r.right(), r.bottom()]
                    .into_iter()
                    .all(oink_display::arrangement::valid_integer)
                || r.w <= 0.0
                || r.h <= 0.0
                || !screen.scale.is_finite()
                || screen.scale <= 0.0
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

/// One restore per departure/return, scoped to a saved revision. Manual saves
/// replace that revision, discarding any excursion belonging to its predecessor.
#[derive(Clone, Debug, Default)]
pub struct RestoreTracker {
    revision: String,
    departed: bool,
    cancelled: bool,
}
impl RestoreTracker {
    pub fn manual_layout(&mut self, profile: &model::Profile) {
        let revision = format!("{}:{}", profile.id, profile.content_revision.token());
        // A new catalog revision may already contain this manual layout. If
        // the revision is still the one we observed before the edit, fence it
        // until the companion save arrives, including across periodic ticks.
        self.cancelled = self.revision == revision;
        self.revision = revision;
        self.departed = false;
    }
    pub fn observe(&mut self, profile: &model::Profile, current: Option<&PhysicalTopology>) -> bool {
        let revision = format!("{}:{}", profile.id, profile.content_revision.token());
        if self.revision != revision {
            self.revision = revision;
            self.departed = false;
            self.cancelled = false;
        }
        if self.cancelled {
            return false;
        }
        let (Some(saved), Some(current)) = (&profile.saved_topology, current) else {
            return false;
        };
        if saved != current {
            self.departed = true;
            return false;
        }
        std::mem::take(&mut self.departed)
    }
}

impl Node {
    pub(super) fn restore_matching_topology(&self) -> Result<()> {
        {
            let data = lock(&self.inner.profiles.data);
            if !data.catalog.as_ref().is_some_and(|c| {
                c.active().saved_topology.is_some() && c.active().content_revision.writer.0 == self.id()
            }) {
                return Ok(());
            }
        }
        let observation = self.observe_profiles()?;
        let config = self.link_config().map_err(super::state::unavailable)?;
        let applied = config.applied();
        if observation.layout_version != applied.version
            || observation
                .physical_topology
                .as_ref()
                .is_none_or(|t| !matches_native(t, &observation.snapshot))
        {
            return Ok(());
        }
        let (target, next, _command, _gate) =
            {
                let mut data = lock(&self.inner.profiles.data);
                if data.pending_layout.is_some()
                    || data
                        .operations
                        .iter()
                        .any(|op| matches!(op.status, OperationStatus::Running | OperationStatus::OutcomeUnknown))
                {
                    return Ok(());
                }
                let Some(catalog) = data.catalog.as_ref() else {
                    return Ok(());
                };
                let profile = catalog.active().clone();
                // Fixed writer authority, no online-dependent election or failover.
                if profile.content_revision.writer.0 != self.id() {
                    return Ok(());
                }
                if config.members().members.iter().any(|m| {
                    !m.is_self && m.online && !config.peer_agreement(m.device).is_some_and(|a| a.contains(CAP))
                }) {
                    return Ok(());
                }
                // A manual layout arrives before its companion catalog on some links.
                // Wait for that catalog; never restore over that intervening edit.
                let new_layout = data.restore_layout_version.as_ref() != Some(&applied.version);
                data.restore_layout_version = Some(applied.version.clone());
                if new_layout && applied.layout_source == dto::LayoutSource::Manual {
                    data.restore_tracker.manual_layout(&profile);
                    return Ok(());
                }
                let mut next = data.restore_tracker.clone();
                if !next.observe(&profile, observation.physical_topology.as_ref()) {
                    data.restore_tracker = next;
                    return Ok(());
                }
                // Ordinary observation must not transiently claim the command gate:
                // that makes unrelated manual IPC commands spuriously return Busy.
                // Reserve only an actual return, and don't consume it if a command
                // or catalog transfer already owns either gate.
                let Ok(command) = self.inner.profiles.command_gate.try_lock() else {
                    return Ok(());
                };
                let Ok(gate) = self.inner.profiles.gate.try_lock() else {
                    return Ok(());
                };
                (profile.content, next, command, gate)
            };
        let restored = config
            .edit_layout_if_current(
                applied,
                Box::new(move |layout| {
                    layout.settings = target.topology;
                    for p in target.placements {
                        layout.place_device(p.device.0, p.origin, p.scale, true);
                    }
                }),
                dto::LayoutSource::Default,
            )
            .map_err(super::state::unavailable)?;
        if restored {
            lock(&self.inner.profiles.data).restore_tracker = next;
        }
        Ok(())
    }
}
