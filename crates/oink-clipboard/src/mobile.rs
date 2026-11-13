//! Metadata-only current index and bounded mobile receive/share policies.
//! Native publication and network authorization remain with the session owner.
use crate::model::OfferId;
use oink_foundation::DeviceId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod source;

pub const SHORT_TEXT_BYTES: usize = 64 * 1024;
pub const SHARE_PNG_BYTES: u64 = 1024 * 1024;
pub const SHARE_PNG_PIXELS: u64 = 4_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeadStatus {
    Readable,
    Cleared,
    Superseded,
    Consumed,
    Disabled,
    Paused,
    Unverified,
    SourceVersionUnsupported,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub offer: OfferId,
    pub boot: [u8; 16],
    pub clock: u64,
    /// Monotone within a copy, including revocation; never reset on reconnect.
    pub revision: u64,
    pub status: HeadStatus,
    pub formats: Vec<String>,
    pub bytes: Option<u64>,
}
impl Head {
    pub fn order(&self) -> (u64, DeviceId) {
        (self.clock, self.offer.clip.origin)
    }
    pub fn valid(&self) -> bool {
        self.formats.len() <= 4 && self.formats.iter().all(|f| f.len() <= 96)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CurrentIndex {
    members: BTreeSet<DeviceId>,
    heads: BTreeMap<DeviceId, Head>,
}
impl CurrentIndex {
    /// The caller supplies PC members of the current group, never mobile shares.
    /// Group replacement must construct a fresh index, even for shared members.
    pub fn new(members: impl IntoIterator<Item = DeviceId>) -> Self {
        Self {
            members: members.into_iter().take(16).collect(),
            heads: BTreeMap::new(),
        }
    }
    pub fn set_members(&mut self, members: impl IntoIterator<Item = DeviceId>) {
        self.members = members.into_iter().take(16).collect();
        self.heads.retain(|id, _| self.members.contains(id));
    }
    /// Authenticated witnesses may report metadata, but never grant body access.
    pub fn observe(&mut self, head: Head) -> bool {
        let id = head.offer.clip.origin;
        if !self.members.contains(&id) || !head.valid() {
            return false;
        }
        if let Some(old) = self.heads.get(&id) {
            if (head.clock, head.revision) <= (old.clock, old.revision) {
                return false;
            }
            // A status update cannot silently change a copy's identity.
            if head.clock == old.clock && (head.offer != old.offer || head.boot != old.boot) {
                return false;
            }
        }
        self.heads.insert(id, head);
        true
    }
    pub fn latest(&self) -> Option<&Head> {
        self.heads.values().max_by_key(|h| h.order())
    }
    pub fn heads(&self) -> impl Iterator<Item = &Head> {
        self.heads.values()
    }
    pub fn restore(&mut self, heads: impl IntoIterator<Item = Head>) {
        for mut head in heads {
            if head.status == HeadStatus::Readable {
                head.status = HeadStatus::Unverified;
            }
            self.observe(head);
        }
    }
    /// Offline is deliberately evaluated after selection: no fallback to older copies.
    pub fn select(&self, source: impl FnOnce(DeviceId) -> SourceState) -> Selection<'_> {
        let Some(head) = self.latest() else {
            return Selection::Unknown;
        };
        if head.status == HeadStatus::SourceVersionUnsupported {
            return Selection::Empty(EmptyReason::SourceVersionUnsupported);
        }
        match source(head.offer.clip.origin) {
            SourceState::Offline => Selection::Empty(EmptyReason::SourceOffline),
            SourceState::Unsupported => Selection::Empty(EmptyReason::SourceVersionUnsupported),
            SourceState::Ready => match head.status {
                HeadStatus::Readable | HeadStatus::Unverified => Selection::Verify(head),
                state => Selection::Empty(EmptyReason::Unavailable(state)),
            },
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceState {
    Ready,
    Offline,
    Unsupported,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyReason {
    SourceOffline,
    SourceVersionUnsupported,
    Unavailable(HeadStatus),
}
#[derive(Debug, PartialEq, Eq)]
pub enum Selection<'a> {
    Unknown,
    Verify(&'a Head),
    Empty(EmptyReason),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoReceive {
    pub enabled: bool,
}
impl AutoReceive {
    pub fn should_read(self, head: &Head, fresh_copy: bool, allowed: bool) -> bool {
        self.enabled
            && fresh_copy
            && allowed
            && head.status == HeadStatus::Readable
            && head.bytes.is_some_and(|n| n <= SHORT_TEXT_BYTES as u64)
            && !head.formats.is_empty()
            && head
                .formats
                .iter()
                .all(|f| matches!(f.as_str(), "text/plain" | "text/uri-list"))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareDescription {
    Text { bytes: u64 },
    Png { bytes: u64, width: u32, height: u32 },
    Other,
}
impl ShareDescription {
    pub fn broadcast_allowed(self) -> bool {
        match self {
            Self::Text { bytes } => bytes <= SHORT_TEXT_BYTES as u64,
            Self::Png { bytes, width, height } => {
                bytes <= SHARE_PNG_BYTES
                    && width > 0
                    && height > 0
                    && u64::from(width) * u64::from(height) <= SHARE_PNG_PIXELS
            }
            Self::Other => false,
        }
    }
}

/// Reliable ordered text framing: offsets and EOF are checked before publication.
#[derive(Debug, Default)]
pub struct TextAssembly {
    bytes: Vec<u8>,
    expected: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    #[error("short text exceeds limit")]
    TooLarge,
    #[error("invalid text stream offset or length")]
    Length,
    #[error("text is not UTF-8")]
    Encoding,
}
impl TextAssembly {
    pub fn new(expected: u64) -> Result<Self, TextError> {
        if expected > SHORT_TEXT_BYTES as u64 {
            return Err(TextError::TooLarge);
        }
        Ok(Self {
            bytes: Vec::new(),
            expected: expected as usize,
        })
    }
    pub fn push(&mut self, offset: u64, bytes: &[u8]) -> Result<(), TextError> {
        if offset != self.bytes.len() as u64 || bytes.len() > 4096 || self.bytes.len() + bytes.len() > self.expected {
            return Err(TextError::Length);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    pub fn finish(self) -> Result<String, TextError> {
        if self.bytes.len() != self.expected {
            return Err(TextError::Length);
        }
        String::from_utf8(self.bytes).map_err(|_| TextError::Encoding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn head(id: u8, clock: u64) -> Head {
        Head {
            offer: OfferId {
                clip: crate::model::ClipId {
                    origin: DeviceId([id; 32]),
                    seq: clock,
                },
                generation: clock,
            },
            boot: [id; 16],
            clock,
            revision: 0,
            status: HeadStatus::Readable,
            formats: vec!["text/plain".into()],
            bytes: Some(1),
        }
    }
    #[test]
    fn latest_offline_never_falls_back() {
        let mut index = CurrentIndex::new([DeviceId([1; 32]), DeviceId([2; 32])]);
        assert!(index.observe(head(1, 9)));
        assert!(index.observe(head(2, 10)));
        assert_eq!(
            index.select(|_| SourceState::Offline),
            Selection::Empty(EmptyReason::SourceOffline)
        );
        assert!(!index.observe(head(2, 8)));
        assert_eq!(index.latest().unwrap().clock, 10);
    }
    #[test]
    fn revocation_beats_replayed_offer_and_survives_restore() {
        let mut index = CurrentIndex::new([DeviceId([1; 32])]);
        let mut revoked = head(1, 10);
        revoked.revision = 1;
        revoked.status = HeadStatus::Consumed;
        index.observe(revoked.clone());
        assert!(!index.observe(head(1, 10)));
        let mut restored = CurrentIndex::new([DeviceId([1; 32])]);
        restored.restore([revoked]);
        assert_eq!(
            restored.select(|_| SourceState::Ready),
            Selection::Empty(EmptyReason::Unavailable(HeadStatus::Consumed))
        );
    }
    #[test]
    fn tie_break_and_restored_authority() {
        let mut index = CurrentIndex::new([DeviceId([1; 32]), DeviceId([2; 32])]);
        index.restore([head(2, u64::MAX), head(1, u64::MAX)]);
        assert_eq!(index.latest().unwrap().offer.clip.origin, DeviceId([2; 32]));
        assert_eq!(index.latest().unwrap().status, HeadStatus::Unverified);
        index.set_members([DeviceId([1; 32])]);
        assert_eq!(index.latest().unwrap().offer.clip.origin, DeviceId([1; 32]));
    }
    #[test]
    fn auto_does_not_backfill_or_fetch_unknown_or_files() {
        let mut h = head(1, 1);
        let auto = AutoReceive { enabled: true };
        assert!(!AutoReceive::default().should_read(&h, true, true));
        assert!(!auto.should_read(&h, false, true));
        assert!(auto.should_read(&h, true, true));
        h.bytes = None;
        assert!(!auto.should_read(&h, true, true));
        h.bytes = Some(65536);
        assert!(auto.should_read(&h, true, true));
        h.bytes = Some(65537);
        assert!(!auto.should_read(&h, true, true));
        h.bytes = Some(1);
        h.formats.push("image/png".into());
        assert!(!auto.should_read(&h, true, true));
    }
    #[test]
    fn share_limits_are_inclusive_and_pixel_multiply_cannot_overflow() {
        assert!(ShareDescription::Text { bytes: 65536 }.broadcast_allowed());
        assert!(!ShareDescription::Text { bytes: 65537 }.broadcast_allowed());
        assert!(
            ShareDescription::Png {
                bytes: 1048576,
                width: 2000,
                height: 2000
            }
            .broadcast_allowed()
        );
        assert!(
            !ShareDescription::Png {
                bytes: 1,
                width: u32::MAX,
                height: u32::MAX
            }
            .broadcast_allowed()
        );
    }
    #[test]
    fn complete_text_only_and_split_utf8() {
        let mut text = TextAssembly::new(3).unwrap();
        text.push(0, &[0xe2]).unwrap();
        assert_eq!(text.push(0, &[1]), Err(TextError::Length));
        text.push(1, &[0x82, 0xac]).unwrap();
        assert_eq!(text.finish().unwrap(), "€");
        assert_eq!(TextAssembly::new(1).unwrap().finish(), Err(TextError::Length));
        assert!(TextAssembly::new(65537).is_err());
    }
}
