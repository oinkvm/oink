//! Explicit sources use portable representations and temporary caller-owned paths.
use super::ShareDescription;
use crate::{
    ClipData, FileReader,
    content::{
        self, Provider, Translator,
        namespace::{Local, Namespace},
    },
    model::*,
};
use oink_foundation::clipboard::ClipPlatform;
use std::{
    collections::HashMap,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
};

#[derive(Clone, Debug)]
pub enum ShareItem {
    Text(String),
    Png(PathBuf),
    File(PathBuf),
}
struct PortableTranslator;
impl Translator for PortableTranslator {
    fn standard(&self, representation: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        Ok(vec![match representation.standard {
            Some(StandardFormat::Text) => {
                ClipData::Text(String::from_utf8(bytes.to_vec()).map_err(|_| io::ErrorKind::InvalidData)?)
            }
            Some(StandardFormat::Png) => ClipData::Png(bytes.to_vec()),
            _ => return Err(io::ErrorKind::Unsupported.into()),
        }])
    }
}
enum Input {
    Text(Vec<u8>),
    File(PathBuf),
}
pub struct ShareSource {
    inventory: Inventory,
    inputs: HashMap<RepresentationId, Input>,
    files: Option<Arc<dyn Namespace>>,
    description: ShareDescription,
    storage: Option<Arc<dyn Send + Sync>>,
}
impl std::fmt::Debug for ShareSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareSource")
            .field("items", &self.inventory.items.len())
            .finish_non_exhaustive()
    }
}
impl ShareSource {
    pub fn new(id: OfferId, items: Vec<ShareItem>) -> io::Result<Arc<Self>> {
        if items.is_empty() || items.len() > 65_535 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let items = if items.iter().all(|item| matches!(item, ShareItem::Text(_))) {
            let mut texts = Vec::with_capacity(items.len());
            for item in items {
                if let ShareItem::Text(text) = item {
                    texts.push(text);
                }
            }
            vec![ShareItem::Text(texts.join("\n"))]
        } else {
            items
        };
        let single = items.len() == 1;
        let mut description = ShareDescription::Other;
        let mut inventory = Inventory {
            id,
            platform: ClipPlatform::Unknown,
            items: vec![],
            roots: vec![],
        };
        let mut inputs = HashMap::new();
        let mut paths = vec![];
        for (index, item) in items.into_iter().enumerate() {
            let rep = RepresentationId(index as u32 + 1);
            let (standard, input, size) = match item {
                ShareItem::Text(text) => {
                    let size = text.len() as u64;
                    if size > REPRESENTATION_BYTES {
                        return Err(io::ErrorKind::FileTooLarge.into());
                    }
                    if single {
                        description = ShareDescription::Text { bytes: size };
                    }
                    (StandardFormat::Text, Input::Text(text.into_bytes()), size)
                }
                ShareItem::Png(path) => {
                    let mut file = std::fs::File::open(&path)?;
                    let size = file.metadata()?.len();
                    if size > REPRESENTATION_BYTES {
                        return Err(io::ErrorKind::FileTooLarge.into());
                    }
                    let mut header = [0; 24];
                    file.read_exact(&mut header)?;
                    if &header[..8] != b"\x89PNG\r\n\x1a\n" || &header[12..16] != b"IHDR" {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    let width = u32::from_be_bytes(header[16..20].try_into().unwrap());
                    let height = u32::from_be_bytes(header[20..24].try_into().unwrap());
                    if width == 0 || height == 0 {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    if single {
                        description = ShareDescription::Png {
                            bytes: size,
                            width,
                            height,
                        };
                    }
                    (StandardFormat::Png, Input::File(path), size)
                }
                ShareItem::File(path) => {
                    paths.push((NodeId(index as u64 + 1), path));
                    continue;
                }
            };
            inputs.insert(rep, input);
            inventory.items.push(ContentItem {
                id: ItemId(index as u32 + 1),
                file: None,
                virtual_files: false,
                representations: vec![Representation {
                    id: rep,
                    format: NativeFormat::Unknown(crate::negotiation::portable_format(standard).into()),
                    medium: Medium::Bytes,
                    aspect: 1,
                    index: -1,
                    standard: Some(standard),
                    data: RepresentationData::Reader {
                        size: ContentSize::Known(size),
                    },
                }],
            });
        }
        let files = if paths.is_empty() {
            None
        } else {
            let local = Local::new(paths)?;
            inventory.roots = local.roots();
            Some(Arc::new(OneShot(local)) as Arc<dyn Namespace>)
        };
        inventory.validate().map_err(content::unavailable)?;
        Ok(Arc::new(Self {
            inventory,
            inputs,
            files,
            description,
            storage: None,
        }))
    }
    pub fn pin_storage(&mut self, storage: Arc<dyn Send + Sync>) {
        self.storage = Some(storage);
    }
    pub fn description(&self) -> ShareDescription {
        self.description
    }
}
impl Provider for ShareSource {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        let reader: FileReader = match self.inputs.get(&id).ok_or(io::ErrorKind::NotFound)? {
            Input::Text(bytes) => Box::new(io::Cursor::new(bytes.clone())),
            Input::File(path) => Box::new(std::fs::File::open(path)?),
        };
        Ok(Box::new(PinnedReader {
            reader,
            _storage: self.storage.clone(),
        }))
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(PortableTranslator)
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        self.files.as_ref().map(|files| {
            Arc::new(PinnedFiles {
                files: files.clone(),
                storage: self.storage.clone(),
            }) as Arc<dyn Namespace>
        })
    }
}
struct PinnedReader {
    reader: FileReader,
    _storage: Option<Arc<dyn Send + Sync>>,
}
impl Read for PinnedReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.reader.read(bytes)
    }
}
struct PinnedFiles {
    files: Arc<dyn Namespace>,
    storage: Option<Arc<dyn Send + Sync>>,
}
impl Namespace for PinnedFiles {
    fn roots(&self) -> Vec<FileNode> {
        self.files.roots()
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.files.one_shot_roots()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.files.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.files.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        Ok(Box::new(PinnedReader {
            reader: self.files.open(node, version, offset)?,
            _storage: self.storage.clone(),
        }))
    }
}
/// Even a normal selected file is a one-shot mobile share, never a permanent link.
struct OneShot(Arc<Local>);
impl Namespace for OneShot {
    fn roots(&self) -> Vec<FileNode> {
        self.0.roots()
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.roots().iter().map(|n| n.id).collect()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.0.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.0.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        self.0.open(node, version, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id() -> OfferId {
        OfferId {
            clip: ClipId {
                origin: oink_foundation::DeviceId([9; 32]),
                seq: 1,
            },
            generation: 1,
        }
    }
    #[test]
    fn multiple_text_items_are_one_bounded_portable_share() {
        let source = ShareSource::new(
            id(),
            vec![ShareItem::Text("Hello".into()), ShareItem::Text("世界".into())],
        )
        .unwrap();
        assert_eq!(source.description(), ShareDescription::Text { bytes: 12 });
        for platform in [ClipPlatform::Mac, ClipPlatform::Windows] {
            let mut reader = content::open_for(source.as_ref(), RepresentationId(1), platform).unwrap();
            assert_eq!(content::read_all(&mut reader).unwrap(), "Hello\n世界".as_bytes());
        }
        assert!(
            ShareSource::new(
                id(),
                vec![ShareItem::Text("a".repeat(65535)), ShareItem::Text(String::new())]
            )
            .unwrap()
            .description()
            .broadcast_allowed()
        );
        assert!(
            !ShareSource::new(
                id(),
                vec![ShareItem::Text("a".repeat(65536)), ShareItem::Text(String::new())]
            )
            .unwrap()
            .description()
            .broadcast_allowed()
        );
    }
    #[test]
    fn source_storage_waits_for_last_active_reader() {
        let storage = Arc::new(tempfile::tempdir().unwrap());
        let path = storage.path().join("shared.bin");
        std::fs::write(&path, b"owned").unwrap();
        let directory = storage.path().to_owned();
        let mut source = ShareSource::new(id(), vec![ShareItem::File(path)]).unwrap();
        Arc::get_mut(&mut source).unwrap().pin_storage(storage.clone());
        let files = source.namespace().unwrap();
        let node = files.select(NodeId(1)).unwrap();
        let mut reader = files.open(node.id, node.version, 0).unwrap();
        drop(storage);
        drop(source);
        drop(files);
        assert!(directory.exists());
        assert_eq!(content::read_all(&mut reader).unwrap(), b"owned");
        drop(reader);
        assert!(!directory.exists());
    }
    #[test]
    fn files_are_one_shot_and_mixed_text_is_not_discarded() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("item.txt");
        std::fs::write(&path, b"file").unwrap();
        let source = ShareSource::new(
            id(),
            vec![ShareItem::Text("caption".into()), ShareItem::File(path.clone())],
        )
        .unwrap();
        assert_eq!(source.description(), ShareDescription::Other);
        assert_eq!(source.inventory().items.len(), 1);
        assert_eq!(source.inventory().roots.len(), 1);
        assert_eq!(source.namespace().unwrap().one_shot_roots(), vec![NodeId(2)]);
        drop(source);
        assert_eq!(std::fs::read(path).unwrap(), b"file");
    }
}
