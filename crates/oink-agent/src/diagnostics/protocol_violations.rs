//! Scoped numeric failures. Endpoint identities become collection-local aliases;
//! no payload bytes, addresses or human error messages are retained.
use super::*;
use oink_protocol::v1::{admission::ProtocolViolation, schema::WireDto};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issue {
    pub code: u16,
    pub scope: u32,
    pub peer_alias: String,
    pub boot_alias: String,
    pub path_alias: Option<String>,
    pub kind: Option<u16>,
    pub type_id: Option<u16>,
    pub capability: Option<String>,
    pub experimental: bool,
    pub isolation: u32,
    pub at_us: u64,
    pub suppressed: u64,
}
impl Recorder {
    pub(crate) fn protocol_violation(&self, peer: DeviceId, violation: &ProtocolViolation) {
        let anon = self.anonymizer();
        let projection = (|| -> oink_protocol::v1::Result<Issue> {
            Ok(Issue {
                code: u16::try_from(violation.code.number())
                    .map_err(|_| oink_protocol::v1::ErrorCode::UnsupportedValue)?,
                scope: violation.scope.number(),
                peer_alias: anon.device(peer),
                boot_alias: anon.router_instance(&violation.boot.nonce()?.0),
                path_alias: violation
                    .path
                    .as_ref()
                    .map(|path| path.encode(4096).map(|bytes| anon.control_identity(&bytes)))
                    .transpose()?,
                kind: violation.kind,
                type_id: violation.type_id,
                capability: violation.capability.clone(),
                experimental: violation.experimental,
                isolation: violation.isolation.number(),
                at_us: violation.at_us,
                suppressed: violation.suppressed,
            })
        })();
        match projection {
            Ok(issue) => self.record(Event::ProtocolViolation(issue)),
            Err(_) => {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_protocol::v1::{self, dto, schema::WireDto};
    #[test]
    fn violations_and_volume_refusals_use_aliases_and_numeric_catalogues_only() {
        let dir = tempfile::tempdir().unwrap();
        let me = DeviceId([7; 32]);
        let peer = DeviceId([8; 32]);
        let recorder = super::super::tests::isolated(dir.path().into(), me, Some(GroupId([9; 16])), true).unwrap();
        recorder.wait_recovery();
        let violation = ProtocolViolation {
            code: v1::ErrorCode::MalformedFrame,
            scope: dto::ErrorScope::LowOperation,
            peer: oink_network::wire_identity::device(peer),
            boot: oink_network::wire_identity::boot(oink_foundation::session::Boot {
                device_id: peer,
                nonce: [33; 16],
            }),
            path: None,
            kind: Some(8),
            type_id: Some(0x0a05),
            capability: None,
            experimental: false,
            isolation: dto::Isolation::LowOperation,
            at_us: 10,
            suppressed: 2,
        };
        recorder.protocol_violation(peer, &violation);
        recorder.volume_refused(
            me,
            peer,
            42,
            oink_routing::input::VolumeKey::Mute,
            dto::ControlFailure::StaleBoot,
        );
        let before = recorder.inner.dropped.load(Ordering::Relaxed);
        let mut wide = violation;
        wide.code = v1::ErrorCode::Unknown(65536);
        recorder.protocol_violation(peer, &wide);
        assert_eq!(recorder.inner.dropped.load(Ordering::Relaxed), before + 1);
        let mut store = lock(&recorder.inner.store);
        recorder.drain(&mut store).unwrap();
        let mut kinds = std::collections::BTreeSet::new();
        for meta in &store.segments {
            for row in super::super::store::decode(&store.read(meta).unwrap()).unwrap().rows {
                let event = row.event.network(true).unwrap();
                let bytes = event.encode(MAX_SEGMENT_RAW).unwrap();
                assert!(!String::from_utf8_lossy(&bytes).contains(&peer.to_hex()));
                assert!(!bytes.windows(32).any(|w| w == peer.0));
                assert!(!bytes.windows(16).any(|w| w == [33; 16]));
                if event.kind().unwrap().0 == "protocol-violation/1" {
                    let v = dto::DiagnosticProtocolViolation::from_raw(event.fields().unwrap()).unwrap();
                    assert_eq!(v.code().unwrap(), 4);
                    assert_eq!(v.suppressed().unwrap(), 2);
                    assert_eq!(v.peer_alias().unwrap().0, recorder.anonymizer().device(peer));
                }
                if event.kind().unwrap().0 == "volume-refused/1" {
                    let v = dto::VolumeRefused::from_raw(event.fields().unwrap()).unwrap();
                    assert_eq!(v.press_id().unwrap(), 42);
                    assert_eq!(v.action().unwrap(), dto::VolumeKey::Mute);
                    assert_eq!(v.reason().unwrap(), dto::ControlFailure::StaleBoot);
                }
                kinds.insert(event.kind().unwrap().0);
            }
        }
        assert!(kinds.contains("protocol-violation/1") && kinds.contains("volume-refused/1"));
        drop(store);
        recorder.shutdown();
    }
}
