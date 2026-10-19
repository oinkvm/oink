use super::remote::tree;
use super::*;
use crate::model::FileNode;
use oink_protocol::v1::{
    self, RawMap, RawValue as V, dto,
    schema::{List, Text, WireDto, WireValue},
};

const MANIFEST_MAX: usize = 16 * 1024 * 1024;
pub(crate) struct Manifest {
    pub inventory: dto::Inventory,
    pub target: ClipPlatform,
    pub nodes: Vec<FileNode>,
    pub files: Vec<tree::FileRecord>,
    pub representations: Vec<(RepresentationId, u64, [u8; 32])>,
}
fn manifest_invalid() -> v1::Error {
    v1::ErrorCode::MalformedFrame.into()
}
fn strict(raw: &RawMap, count: u32) -> v1::Result<()> {
    if raw.len() != count as usize || raw.keys().copied().ne(1..=count) {
        return Err(manifest_invalid());
    }
    Ok(())
}
fn read<T: WireValue>(raw: &RawMap, key: u32) -> v1::Result<T> {
    T::read(raw.get(&key).ok_or_else(manifest_invalid)?)
}
impl Manifest {
    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        let convert = || -> v1::Result<Vec<u8>> {
            let mut representations = self.representations.clone();
            representations.sort_by_key(|r| r.0.0);
            let nodes = self
                .nodes
                .iter()
                .map(|n| crate::wire::node(n).map(|n| n.write()))
                .collect::<v1::Result<Vec<_>>>()?;
            let files = self
                .files
                .iter()
                .map(|file| {
                    V::Map(
                        [
                            (1, V::Uint(file.node.0)),
                            (2, V::Uint(file.version)),
                            (3, V::Uint(file.size)),
                            (4, crate::wire::digest(file.hash).write()),
                        ]
                        .into(),
                    )
                })
                .collect();
            let reps = representations
                .iter()
                .map(|(id, size, hash)| {
                    V::Map(
                        [
                            (1, V::Uint(id.0.into())),
                            (2, V::Uint(*size)),
                            (3, crate::wire::digest(*hash).write()),
                        ]
                        .into(),
                    )
                })
                .collect();
            v1::cbor::encode_map(
                &[
                    (1, V::Uint(1)),
                    (2, self.inventory.write()),
                    (3, V::Text(crate::wire::platform(self.target).into())),
                    (4, V::Array(nodes)),
                    (5, V::Array(files)),
                    (6, V::Array(reps)),
                ]
                .into(),
                MANIFEST_MAX,
            )
        };
        convert().map_err(|_| unavailable(Unavailable::InvalidData))
    }
    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        let convert = || -> v1::Result<Self> {
            let raw = v1::cbor::decode_map(bytes, MANIFEST_MAX)?;
            strict(&raw, 6)?;
            if read::<u64>(&raw, 1)? != 1 {
                return Err(manifest_invalid());
            }
            let inventory = read(&raw, 2)?;
            let target = match read::<Text<16>>(&raw, 3)?.0.as_str() {
                "macos" => ClipPlatform::Mac,
                "windows" => ClipPlatform::Windows,
                _ => return Err(manifest_invalid()),
            };
            let nodes = read::<List<dto::FileNode, 100000>>(&raw, 4)?
                .0
                .into_iter()
                .map(|node| {
                    if node.raw().keys().any(|key| !(1..=7).contains(key)) {
                        return Err(manifest_invalid());
                    }
                    crate::wire::local_node(node)
                })
                .collect::<v1::Result<Vec<_>>>()?;
            if nodes.len() > crate::model::MAX_DISCOVERED_NODES {
                return Err(v1::ErrorCode::LimitExceeded.into());
            }
            let files = read::<List<RawMap, 100000>>(&raw, 5)?
                .0
                .into_iter()
                .map(|file| {
                    strict(&file, 4)?;
                    Ok(tree::FileRecord {
                        node: crate::model::NodeId(read(&file, 1)?),
                        version: read(&file, 2)?,
                        size: read(&file, 3)?,
                        hash: crate::wire::local_digest(read(&file, 4)?)?,
                    })
                })
                .collect::<v1::Result<Vec<_>>>()?;
            let representations = read::<List<RawMap, 65535>>(&raw, 6)?
                .0
                .into_iter()
                .map(|rep| {
                    strict(&rep, 3)?;
                    Ok((
                        RepresentationId(read(&rep, 1)?),
                        read(&rep, 2)?,
                        crate::wire::local_digest(read(&rep, 3)?)?,
                    ))
                })
                .collect::<v1::Result<Vec<_>>>()?;
            Ok(Self {
                inventory,
                target,
                nodes,
                files,
                representations,
            })
        };
        convert().map_err(|_| unavailable(Unavailable::InvalidData))
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    fn fixture() -> Manifest {
        let inventory = crate::wire::inventory(&super::super::tests::fixture().inventory()).unwrap();
        Manifest {
            inventory,
            target: ClipPlatform::Mac,
            nodes: vec![],
            files: vec![],
            representations: vec![],
        }
    }
    #[test]
    fn shared_inventory_unknowns_survive_manifest_and_delivery_outcome_update() {
        let mut manifest = fixture();
        let mut raw = manifest.inventory.raw().clone();
        raw.insert(90, V::Map([(7, V::Bytes(vec![0, 255]))].into()));
        let V::Array(items) = raw.get_mut(&3).unwrap() else {
            panic!()
        };
        let V::Map(item) = &mut items[0] else { panic!() };
        item.insert(91, V::Text("future item".into()));
        let V::Array(reps) = item.get_mut(&3).unwrap() else {
            panic!()
        };
        let V::Map(rep) = &mut reps[0] else { panic!() };
        rep.insert(92, V::Bool(true));
        manifest.inventory = dto::Inventory::from_raw(raw).unwrap();
        let mut local = crate::wire::local_inventory(&manifest.inventory).unwrap();
        local.items[0].representations[0].data = RepresentationData::Unavailable(Unavailable::LimitExceeded);
        manifest.inventory = crate::wire::updated_inventory(&manifest.inventory, &local).unwrap();
        let bytes = manifest.encode().unwrap();
        let restored = Manifest::decode(&bytes).unwrap();
        assert_eq!(restored.encode().unwrap(), bytes);
        assert_eq!(
            restored.inventory.raw()[&90],
            V::Map([(7, V::Bytes(vec![0, 255]))].into())
        );
        let item = &restored.inventory.items().unwrap().0[0];
        assert_eq!(item.raw()[&91], V::Text("future item".into()));
        let rep = &item.representations().unwrap().0[0];
        assert_eq!(rep.raw()[&92], V::Bool(true));
        assert_eq!(
            crate::wire::local_inventory(&restored.inventory).unwrap().items[0].representations[0].data,
            RepresentationData::Unavailable(Unavailable::LimitExceeded)
        );
    }
    #[test]
    fn unknown_schema_execution_fields_and_noncanonical_bytes_are_refused() {
        let bytes = fixture().encode().unwrap();
        for (key, value) in [(1, V::Uint(2)), (90, V::Bool(true))] {
            let mut raw = v1::cbor::decode_map(&bytes, MANIFEST_MAX).unwrap();
            raw.insert(key, value);
            assert!(Manifest::decode(&v1::cbor::encode_map(&raw, MANIFEST_MAX).unwrap()).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Manifest::decode(&trailing).is_err());
        assert!(Manifest::decode(&bytes[..bytes.len() - 1]).is_err());
    }
}
