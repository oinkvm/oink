//! Local clipboard preferences and live revocation of existing readers.
use crate::content::{self, Provider, Translator, namespace::Namespace};
#[cfg(test)]
use crate::model::{ContentItem, ContentSize, ItemId, Medium, NativeFormat, Representation, StandardFormat};
use crate::model::{
    DirectoryCursor, DirectoryPage, FileNode, Inventory, NodeId, RepresentationData, RepresentationId, Unavailable,
};
use crate::{ClipData, FileReader};
#[cfg(test)]
use oink_foundation::content::OfferId;
use serde::{Deserialize, Serialize};
use std::{
    io,
    sync::{Arc, RwLock},
    time::Duration,
};

/// Device-local direction and source preferences. All format classes are shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Device-local direction switches; omitted by pre-0.2 settings.
    #[serde(default = "enabled_by_default")]
    pub receive: bool,
    #[serde(default = "enabled_by_default")]
    pub send: bool,
    /// Local source preference; never part of the group configuration.
    #[serde(default = "enabled_by_default")]
    pub remove_ds_store: bool,
}
fn enabled_by_default() -> bool {
    true
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            receive: true,
            send: true,
            remove_ds_store: true,
        }
    }
}
impl Settings {
    /// Only this source Mac's folder children are filtered for a different OS.
    pub fn omit_ds_store_for(self, source_os: &str, target_os: &str) -> bool {
        self.remove_ds_store && source_os == "macos" && !target_os.is_empty() && target_os != "macos"
    }
}

/// Remove file representations only when native file delivery is unavailable.
/// This is a capability projection, independent of clipboard preferences.
pub fn without_files(mut inventory: Inventory) -> Inventory {
    for item in &mut inventory.items {
        item.representations.retain(|rep| !file_representation(rep));
        item.file = None;
    }
    inventory.items.retain(|item| !item.representations.is_empty());
    inventory.roots.clear();
    inventory
}
fn file_representation(rep: &crate::model::Representation) -> bool {
    matches!(rep.data, RepresentationData::File(_) | RepresentationData::Promise(..))
}
pub fn items_without_files(items: Vec<ClipData>) -> Vec<ClipData> {
    items
        .into_iter()
        .filter_map(|mut data| {
            let keep = match &mut data {
                ClipData::Files(_) | ClipData::FileRoots(_) => false,
                ClipData::Native { item, .. } => {
                    item.representations.retain(|rep| !file_representation(rep));
                    item.file = None;
                    !item.representations.is_empty()
                }
                _ => true,
            };
            keep.then_some(data)
        })
        .collect()
}

#[derive(Clone, Debug)]
struct State {
    enabled: bool,
    epoch: Arc<()>,
}
/// Shared by the owner switch, source advertisements and readers already handed
/// to native consumers. Every read checks current permission before and after IO.
#[derive(Clone, Debug)]
pub struct SharingPolicy(Arc<RwLock<State>>);
impl SharingPolicy {
    pub fn new(enabled: bool) -> Self {
        Self(Arc::new(RwLock::new(State {
            enabled,
            epoch: Arc::new(()),
        })))
    }
    pub fn enabled(&self) -> bool {
        self.0.read().unwrap().enabled
    }
    /// Reconfiguration retires readers in both directions, even when only the
    /// other direction or the source's .DS_Store preference changed.
    pub fn update(&self, enabled: bool) {
        *self.0.write().unwrap() = State {
            enabled,
            epoch: Arc::new(()),
        };
    }
    /// A deferred publication cannot regain permission after disable/re-enable
    /// even when its payload was already prepared.
    pub fn publication_guard(&self) -> content::ReaderGuard {
        let epoch = self.0.read().unwrap().epoch.clone();
        let policy = self.clone();
        Arc::new(move || {
            let state = policy.0.read().unwrap();
            if state.enabled && Arc::ptr_eq(&state.epoch, &epoch) {
                Ok(())
            } else {
                Err(content::unavailable(Unavailable::Permission))
            }
        })
    }
    pub fn provider(&self, provider: Arc<dyn Provider>) -> Arc<dyn Provider> {
        Arc::new(Filtered {
            provider,
            policy: self.clone(),
        })
    }
    fn inventory(&self, mut inventory: Inventory) -> Inventory {
        let state = self.0.read().unwrap();
        if !state.enabled {
            inventory.items.clear();
            inventory.roots.clear();
        }
        inventory
    }
    fn check(&self) -> io::Result<()> {
        if self.enabled() {
            Ok(())
        } else {
            Err(content::unavailable(Unavailable::Permission))
        }
    }
    fn files_guard(&self) -> content::ReaderGuard {
        let epoch = self.0.read().unwrap().epoch.clone();
        let policy = self.clone();
        Arc::new(move || {
            if !Arc::ptr_eq(&policy.0.read().unwrap().epoch, &epoch) {
                return Err(content::unavailable(Unavailable::Permission));
            }
            policy.check()
        })
    }
}
struct Filtered {
    provider: Arc<dyn Provider>,
    policy: SharingPolicy,
}
impl Filtered {
    fn reader(&self, id: RepresentationId, open: impl FnOnce() -> io::Result<FileReader>) -> io::Result<FileReader> {
        let guard = self.reader_guard(id)?;
        guard()?;
        content::guarded(open()?, guard)
    }
}
impl Provider for Filtered {
    fn inventory(&self) -> Inventory {
        self.policy.inventory(self.provider.inventory())
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        self.reader(id, || self.provider.open(id))
    }
    fn open_hover(&self, id: RepresentationId) -> io::Result<FileReader> {
        self.reader(id, || self.provider.open_hover(id))
    }
    fn open_preview(&self, id: RepresentationId, timeout: Duration) -> io::Result<FileReader> {
        self.reader(id, || self.provider.open_preview(id, timeout))
    }
    fn translator(&self) -> Arc<dyn Translator> {
        self.provider.translator()
    }
    fn reader_guard(&self, id: RepresentationId) -> io::Result<content::ReaderGuard> {
        let inventory = self.provider.inventory();
        content::find(&inventory, id).ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
        self.policy.check()?;
        let epoch = self.policy.0.read().unwrap().epoch.clone();
        let inner = self.provider.reader_guard(id)?;
        let policy = self.policy.clone();
        Ok(Arc::new(move || {
            inner()?;
            if !Arc::ptr_eq(&policy.0.read().unwrap().epoch, &epoch) {
                return Err(content::unavailable(Unavailable::Permission));
            }
            policy.check()
        }))
    }

    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        self.policy.check().ok()?;
        self.provider.namespace().map(|inner| {
            Arc::new(FilteredFiles {
                inner,
                policy: self.policy.clone(),
            }) as Arc<dyn Namespace>
        })
    }
    fn local_batch(&self) -> Option<crate::store::Batch> {
        self.policy.enabled().then(|| self.provider.local_batch()).flatten()
    }
    fn cancel(&self) {
        self.provider.cancel();
    }
}
struct FilteredFiles {
    inner: Arc<dyn Namespace>,
    policy: SharingPolicy,
}
impl Namespace for FilteredFiles {
    fn release_fulfilled(&self, roots: &[NodeId]) {
        self.inner.release_fulfilled(roots);
    }
    fn native_file_metadata(&self) -> content::native_metadata::NativeFileMetadata {
        if self.policy.check().is_ok() {
            self.inner.native_file_metadata()
        } else {
            Default::default()
        }
    }
    fn clipboard_key(&self) -> io::Result<Option<[u8; 32]>> {
        self.inner.clipboard_key()
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        if self.policy.check().is_ok() {
            self.inner.one_shot_roots()
        } else {
            Vec::new()
        }
    }
    fn requires_native_files(&self) -> bool {
        self.inner.requires_native_files()
    }
    fn source_roots(&self) -> io::Result<Vec<content::grants::SourceRoot>> {
        let guard = self.policy.files_guard();
        guard()?;
        let roots = self.inner.source_roots()?;
        guard()?;
        Ok(roots)
    }
    fn prepare(&self) -> io::Result<()> {
        self.policy.check()?;
        self.inner.prepare()
    }
    fn native_lease(&self) -> io::Result<content::namespace::NativeLease> {
        self.policy.check()?;
        self.inner.native_lease()
    }
    fn native_consumed(&self, nodes: &[crate::model::NodeId]) -> io::Result<()> {
        self.policy.check()?;
        self.inner.native_consumed(nodes)
    }

    fn bind_native(&self, native: std::sync::Weak<dyn content::namespace::NativeFiles>) -> io::Result<()> {
        self.policy.check()?;
        self.inner.bind_native(native)
    }
    fn native_list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> Option<io::Result<DirectoryPage>> {
        let guard = self.policy.files_guard();
        if let Err(error) = guard() {
            return Some(Err(error));
        }
        self.inner.native_list(node, cursor).map(|page| {
            let page = page?;
            guard()?;
            Ok(page)
        })
    }
    fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<FileReader>> {
        let guard = self.policy.files_guard();
        if let Err(error) = guard() {
            return Some(Err(error));
        }
        self.inner
            .native_open(node, offset)
            .map(|reader| content::guarded(reader?, guard))
    }
    fn native_metadata(&self, node: NodeId) -> Option<io::Result<crate::FileMetadata>> {
        if let Err(error) = self.policy.check() {
            return Some(Err(error));
        }
        self.inner.native_metadata(node)
    }
    fn source_link(&self) -> Option<content::linked::Descriptor> {
        self.policy.check().ok()?;
        self.inner.source_link()
    }
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        self.inner.origin()
    }
    fn real_clipboard_files(&self) -> bool {
        self.inner.real_clipboard_files()
    }
    fn retain_files(&self, retention: content::FileRetention) {
        self.inner.retain_files(retention);
    }
    fn roots(&self) -> Vec<FileNode> {
        if self.policy.check().is_ok() {
            self.inner.roots()
        } else {
            vec![]
        }
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        let guard = self.policy.files_guard();
        guard()?;
        let page = self.inner.list(node, cursor)?;
        guard()?;
        Ok(page)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        let guard = self.policy.files_guard();
        guard()?;
        let selected = self.inner.select(node)?;
        guard()?;
        Ok(selected)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        let guard = self.policy.files_guard();
        guard()?;
        content::guarded(self.inner.open(node, version, offset)?, guard)
    }

    fn cancel(&self) {
        self.inner.cancel();
    }
    fn preparing(&self) {
        if self.policy.check().is_ok() {
            self.inner.preparing();
        }
    }
    fn in_use(&self) -> bool {
        self.inner.in_use()
    }
    fn local_batch(&self) -> Option<crate::store::Batch> {
        self.policy.check().ok()?;
        self.inner.local_batch()
    }
    fn cache_batch(&self) -> Option<crate::store::Batch> {
        self.policy.check().ok()?;
        self.inner.cache_batch()
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn clipboard_masters_default_on_for_existing_config_and_are_independent() {
        let old = r#"{"text":true,"rich_text":true,"images":true,"files":true,"custom":true}"#;
        let settings: super::Settings = serde_json::from_str(old).unwrap();
        let mut value = serde_json::to_value(settings).unwrap();
        assert_eq!(value["receive"], true);
        assert_eq!(value["send"], true);
        value["send"] = false.into();
        let decoded: super::Settings = serde_json::from_value(value).unwrap();
        let value = serde_json::to_value(decoded).unwrap();
        assert_eq!(value["receive"], true);
        assert_eq!(value["send"], false);
    }

    use super::*;
    use oink_foundation::content::ClipId;
    use oink_foundation::{DeviceId, clipboard::ClipPlatform};
    use std::io::Read as _;
    fn inventory() -> Inventory {
        Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([1; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Mac,
            roots: vec![],
            items: vec![ContentItem {
                virtual_files: false,
                id: ItemId(1),
                file: None,
                representations: [
                    Some(StandardFormat::Text),
                    Some(StandardFormat::Html),
                    Some(StandardFormat::Rtf),
                    Some(StandardFormat::Png),
                    Some(StandardFormat::Privacy),
                    None,
                ]
                .into_iter()
                .enumerate()
                .map(|(index, standard)| Representation {
                    id: RepresentationId(index as u32 + 1),
                    format: NativeFormat::Mac(format!("example.{index}")),
                    medium: Medium::Bytes,
                    aspect: 1,
                    index: -1,
                    standard,
                    data: RepresentationData::Reader {
                        size: ContentSize::Known(4),
                    },
                })
                .collect(),
            }],
        }
    }
    struct Source;
    struct Translator;
    impl content::Translator for Translator {
        fn standard(&self, _: &Representation, _: &[u8]) -> io::Result<Vec<ClipData>> {
            Err(content::unavailable(Unavailable::Unsupported))
        }
    }
    impl Provider for Source {
        fn inventory(&self) -> Inventory {
            inventory()
        }
        fn open(&self, _: RepresentationId) -> io::Result<FileReader> {
            Ok(Box::new(io::Cursor::new(b"data")))
        }
        fn translator(&self) -> Arc<dyn content::Translator> {
            Arc::new(Translator)
        }
    }
    struct Files;
    impl Namespace for Files {
        fn origin(&self) -> Option<DeviceId> {
            Some(DeviceId([7; 32]))
        }
        fn roots(&self) -> Vec<FileNode> {
            vec![file()]
        }
        fn list(&self, _: NodeId, _: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
            Err(content::unavailable(Unavailable::NoSuchItem))
        }
        fn select(&self, _: NodeId) -> io::Result<FileNode> {
            Ok(file())
        }
        fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<FileReader> {
            Ok(Box::new(io::Cursor::new(b"file")))
        }
    }
    fn file() -> FileNode {
        FileNode {
            exec_bits: None,
            id: NodeId(1),
            parent: None,
            name: "file".into(),
            kind: crate::model::NodeKind::File,
            size: ContentSize::Known(4),
            version: 1,
            modified_ms: None,
        }
    }
    struct Inline;
    impl Provider for Inline {
        fn inventory(&self) -> Inventory {
            let mut inventory = inventory();
            inventory.items[0].representations.truncate(1);
            inventory.items[0].representations[0].data = RepresentationData::Inline(b"data".to_vec());
            inventory
        }
        fn open(&self, _: RepresentationId) -> io::Result<FileReader> {
            panic!("inline must not reopen storage")
        }
        fn translator(&self) -> Arc<dyn content::Translator> {
            Arc::new(PortableText)
        }
    }
    struct PortableText;
    impl content::Translator for PortableText {
        fn standard(&self, _: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
            Ok(vec![ClipData::Text(String::from_utf8(bytes.to_vec()).unwrap())])
        }
    }
    #[test]
    fn prepared_publication_does_not_regain_authority_after_reenable() {
        let policy = SharingPolicy::new(true);
        let before = policy.publication_guard();
        assert!(before().is_ok());
        policy.update(false);
        policy.update(true);
        assert!(before().is_err());
        assert!(policy.publication_guard()().is_ok());
    }
    #[test]
    fn inline_and_translated_readers_stay_retired_after_reenable() {
        for target in [
            oink_foundation::clipboard::ClipPlatform::Mac,
            oink_foundation::clipboard::ClipPlatform::Windows,
        ] {
            let policy = SharingPolicy::new(true);
            let source = policy.provider(Arc::new(Inline));
            let mut reader = content::open_for(source.as_ref(), RepresentationId(1), target).unwrap();
            assert_eq!(reader.read(&mut [0; 1]).unwrap(), 1);
            policy.update(false);
            policy.update(true);
            assert!(
                reader.read(&mut [0; 1]).is_err(),
                "an interrupted reader cannot resume with new permission"
            );
            assert!(
                content::open_for(source.as_ref(), RepresentationId(1), target).is_ok(),
                "a fresh read uses current permission"
            );
        }
    }
    #[test]
    fn disabling_sharing_revokes_a_namespace_and_active_file_reader() {
        let policy = SharingPolicy::new(true);
        let files = FilteredFiles {
            inner: Arc::new(Files),
            policy: policy.clone(),
        };
        assert_eq!(
            files.origin(),
            Some(DeviceId([7; 32])),
            "format policy lost the original sending device"
        );
        let mut reader = files.open(NodeId(1), 1, 0).unwrap();
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 1);
        policy.update(false);
        assert!(files.roots().is_empty());
        assert!(files.select(NodeId(1)).is_err());
        assert!(files.list(NodeId(1), None).is_err());
        assert!(files.open(NodeId(1), 1, 0).is_err());
        assert!(reader.read(&mut [0; 1]).is_err());
    }
    #[test]
    fn all_formats_and_files_are_available_despite_old_stored_filters() {
        let settings: Settings =
            serde_json::from_str(r#"{"text":false,"rich_text":false,"images":false,"files":false,"custom":false}"#)
                .unwrap();
        assert_eq!(settings, Settings::default());
        let stored = serde_json::to_value(settings).unwrap();
        for key in ["text", "rich_text", "images", "files", "custom"] {
            assert!(stored.get(key).is_none());
        }
        let policy = SharingPolicy::new(settings.send);
        let source = policy.provider(Arc::new(Source));
        assert_eq!(source.inventory(), inventory());
        for id in 1..=6 {
            let mut bytes = Vec::new();
            source
                .open(RepresentationId(id))
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes, b"data");
        }
        let files = FilteredFiles {
            inner: Arc::new(Files),
            policy: policy.clone(),
        };
        assert_eq!(files.roots(), vec![file()]);
        let mut bytes = Vec::new();
        files.open(NodeId(1), 1, 0).unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"file");
        let mut reader = source.open(RepresentationId(1)).unwrap();
        policy.update(false);
        assert!(source.inventory().items.is_empty());
        assert!(source.open(RepresentationId(1)).is_err());
        assert!(reader.read(&mut [0; 1]).is_err());
    }
}
