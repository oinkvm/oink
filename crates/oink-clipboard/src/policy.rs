//! Local format selection and live revocation of existing clipboard readers.
use crate::content::{self, Provider, Translator, namespace::Namespace};
#[cfg(test)]
use crate::model::{ContentItem, ContentSize, ItemId, Medium, NativeFormat};
use crate::model::{
    DirectoryCursor, DirectoryPage, FileNode, Inventory, NodeId, Representation, RepresentationData, RepresentationId,
    StandardFormat, Unavailable,
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

/// Private formats are representations without a recognised standard meaning.
/// Privacy markers have a recognised meaning and are independent of this filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatFilters {
    /// Device-local direction switches; omitted by pre-0.2 settings.
    #[serde(default = "enabled_by_default")]
    pub receive: bool,
    #[serde(default = "enabled_by_default")]
    pub send: bool,
    pub text: bool,
    pub rich_text: bool,
    pub images: bool,
    pub files: bool,
    pub custom: bool,
}
fn enabled_by_default() -> bool {
    true
}
impl Default for FormatFilters {
    fn default() -> Self {
        Self {
            receive: true,
            send: true,
            text: true,
            rich_text: true,
            images: true,
            files: true,
            custom: true,
        }
    }
}
impl FormatFilters {
    fn representation(self, rep: &Representation) -> bool {
        if matches!(rep.data, RepresentationData::File(_) | RepresentationData::Promise(..)) {
            return self.files;
        }
        if crate::content::url::title_format(&rep.format) {
            return self.text;
        }
        match rep.standard {
            Some(StandardFormat::Text | StandardFormat::Url) => self.text,
            Some(StandardFormat::Html | StandardFormat::Rtf) => self.rich_text,
            Some(StandardFormat::Png) => self.images,
            Some(StandardFormat::Privacy) => true,
            None => self.custom,
        }
    }
    pub fn items(self, items: Vec<ClipData>) -> Vec<ClipData> {
        items
            .into_iter()
            .filter_map(|mut data| {
                let keep = match &mut data {
                    ClipData::Text(_) => self.text,
                    ClipData::Html(_) | ClipData::Rtf(_) => self.rich_text,
                    ClipData::Png(_) => self.images,
                    ClipData::Files(_) | ClipData::FileRoots(_) => self.files,
                    ClipData::Custom { platform, name, .. } => {
                        crate::formats::is_marker(*platform, name) || self.custom
                    }
                    ClipData::Native { item, .. } => {
                        item.representations.retain(|rep| self.representation(rep));
                        if !self.files {
                            item.file = None;
                        }
                        !item.representations.is_empty()
                    }
                };
                keep.then_some(data)
            })
            .collect()
    }
    pub fn inventory(self, mut inventory: Inventory) -> Inventory {
        for item in &mut inventory.items {
            item.representations.retain(|rep| self.representation(rep));
            if !self.files {
                item.file = None;
            }
        }
        inventory.items.retain(|item| !item.representations.is_empty());
        if !self.files {
            inventory.roots.clear();
        }
        inventory
    }
}

#[derive(Clone, Debug)]
struct State {
    enabled: bool,
    filters: FormatFilters,
    epoch: Arc<()>,
}
/// Shared by the owner switch, source advertisements and readers already handed
/// to native consumers. Every read checks current permission before and after IO.
#[derive(Clone, Debug)]
pub struct SharingPolicy(Arc<RwLock<State>>);
impl SharingPolicy {
    pub fn new(enabled: bool, filters: FormatFilters) -> Self {
        Self(Arc::new(RwLock::new(State {
            enabled,
            filters,
            epoch: Arc::new(()),
        })))
    }
    pub fn enabled(&self) -> bool {
        self.0.read().unwrap().enabled
    }
    pub fn update(&self, enabled: bool, filters: FormatFilters) {
        let mut state = self.0.write().unwrap();
        if state.enabled != enabled || state.filters != filters {
            *state = State {
                enabled,
                filters,
                epoch: Arc::new(()),
            };
        }
    }
    /// A deferred publication cannot regain permission after disable/re-enable
    /// or a filter change, even when its payload was already prepared.
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
    fn inventory(&self, inventory: Inventory) -> Inventory {
        let state = self.0.read().unwrap();
        let mut inventory = state.filters.inventory(inventory);
        if !state.enabled {
            inventory.items.clear();
            inventory.roots.clear();
        }
        inventory
    }
    fn representation(&self, representation: &Representation) -> io::Result<()> {
        let state = self.0.read().unwrap();
        if state.enabled && state.filters.representation(representation) {
            Ok(())
        } else {
            Err(content::unavailable(Unavailable::Permission))
        }
    }
    fn files(&self) -> io::Result<()> {
        let state = self.0.read().unwrap();
        if state.enabled && state.filters.files {
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
            policy.files()
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
        let representation = content::find(&inventory, id)
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?
            .clone();
        self.policy.representation(&representation)?;
        let epoch = self.policy.0.read().unwrap().epoch.clone();
        let inner = self.provider.reader_guard(id)?;
        let policy = self.policy.clone();
        Ok(Arc::new(move || {
            inner()?;
            if !Arc::ptr_eq(&policy.0.read().unwrap().epoch, &epoch) {
                return Err(content::unavailable(Unavailable::Permission));
            }
            policy.representation(&representation)
        }))
    }

    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        self.policy.files().ok()?;
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
    fn real_clipboard_files(&self) -> bool {
        self.inner.real_clipboard_files()
    }
    fn retain_files(&self, retention: content::FileRetention) {
        self.inner.retain_files(retention);
    }
    fn roots(&self) -> Vec<FileNode> {
        if self.policy.files().is_ok() {
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
        if self.policy.files().is_ok() {
            self.inner.preparing();
        }
    }
    fn in_use(&self) -> bool {
        self.inner.in_use()
    }
    fn on_stored(&self, ready: Box<dyn FnOnce(content::Completed) + Send>) {
        self.inner.on_stored(ready);
    }
    fn local_batch(&self) -> Option<crate::store::Batch> {
        self.policy.files().ok()?;
        self.inner.local_batch()
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn clipboard_masters_default_on_for_existing_config_and_are_independent() {
        let old = r#"{"text":true,"rich_text":true,"images":true,"files":true,"custom":true}"#;
        let settings: super::FormatFilters = serde_json::from_str(old).unwrap();
        let mut value = serde_json::to_value(settings).unwrap();
        assert_eq!(value["receive"], true);
        assert_eq!(value["send"], true);
        value["send"] = false.into();
        let decoded: super::FormatFilters = serde_json::from_value(value).unwrap();
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
                        hash: None,
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
        let policy = SharingPolicy::new(true, FormatFilters::default());
        let before = policy.publication_guard();
        assert!(before().is_ok());
        policy.update(false, FormatFilters::default());
        policy.update(true, FormatFilters::default());
        assert!(before().is_err());
        assert!(policy.publication_guard()().is_ok());
    }
    #[test]
    fn inline_and_translated_readers_stay_retired_after_reenable() {
        for target in [
            oink_foundation::clipboard::ClipPlatform::Mac,
            oink_foundation::clipboard::ClipPlatform::Windows,
        ] {
            let policy = SharingPolicy::new(true, FormatFilters::default());
            let source = policy.provider(Arc::new(Inline));
            let mut reader = content::open_for(source.as_ref(), RepresentationId(1), target).unwrap();
            assert_eq!(reader.read(&mut [0; 1]).unwrap(), 1);
            policy.update(false, FormatFilters::default());
            policy.update(true, FormatFilters::default());
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
    fn file_filter_revokes_a_namespace_and_active_file_reader() {
        let policy = SharingPolicy::new(true, FormatFilters::default());
        let files = FilteredFiles {
            inner: Arc::new(Files),
            policy: policy.clone(),
        };
        let mut reader = files.open(NodeId(1), 1, 0).unwrap();
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 1);
        policy.update(
            true,
            FormatFilters {
                files: false,
                ..Default::default()
            },
        );
        assert!(files.roots().is_empty());
        assert!(files.select(NodeId(1)).is_err());
        assert!(files.list(NodeId(1), None).is_err());
        assert!(files.open(NodeId(1), 1, 0).is_err());
        assert!(reader.read(&mut [0; 1]).is_err());
    }
    #[test]
    fn every_filter_removes_inventory_and_existing_reads_are_revoked() {
        let defaults = FormatFilters::default();
        let policy = SharingPolicy::new(true, defaults);
        let source = policy.provider(Arc::new(Source));
        for (index, disabled) in [
            FormatFilters {
                text: false,
                ..defaults
            },
            FormatFilters {
                rich_text: false,
                ..defaults
            },
            FormatFilters {
                images: false,
                ..defaults
            },
            FormatFilters {
                custom: false,
                ..defaults
            },
        ]
        .into_iter()
        .enumerate()
        {
            policy.update(true, defaults);
            let id = RepresentationId([1, 2, 4, 6][index]);
            let mut reader = source.open(id).unwrap();
            policy.update(true, disabled);
            assert!(content::find(&source.inventory(), id).is_none());
            assert!(source.open(id).is_err(), "an old offer cannot bypass the new filter");
            assert!(reader.read(&mut [0; 1]).is_err(), "an active reader must stop");
            assert!(
                content::find(&source.inventory(), RepresentationId(5)).is_some(),
                "privacy markers remain recognised"
            );
        }
        policy.update(true, defaults);
        let mut reader = source.open(RepresentationId(1)).unwrap();
        policy.update(false, defaults);
        assert!(source.inventory().items.is_empty());
        assert!(source.open(RepresentationId(1)).is_err());
        assert!(reader.read(&mut [0; 1]).is_err());
    }
    #[test]
    fn file_controls_are_files_and_private_formats_are_unrecognised() {
        let defaults = FormatFilters::default();
        let mut inventory = inventory();
        inventory.items[0].representations.push(Representation {
            id: RepresentationId(7),
            format: NativeFormat::Mac("public.file-url".into()),
            medium: Medium::Bytes,
            aspect: 1,
            index: -1,
            standard: None,
            data: RepresentationData::File(NodeId(1)),
        });
        let filtered = FormatFilters {
            custom: false,
            ..defaults
        }
        .inventory(inventory.clone());
        assert!(content::find(&filtered, RepresentationId(7)).is_some());
        assert!(content::find(&filtered, RepresentationId(6)).is_none());
        let filtered = FormatFilters {
            files: false,
            ..defaults
        }
        .inventory(inventory);
        assert!(content::find(&filtered, RepresentationId(7)).is_none());
        assert!(content::find(&filtered, RepresentationId(6)).is_some());
        let privacy = ClipData::Custom {
            platform: ClipPlatform::Mac,
            name: crate::formats::MAC_MARKERS[1].into(),
            bytes: vec![],
        };
        let unknown = ClipData::Custom {
            platform: ClipPlatform::Mac,
            name: "example.private".into(),
            bytes: vec![1],
        };
        assert_eq!(
            FormatFilters {
                custom: false,
                ..defaults
            }
            .items(vec![privacy.clone(), unknown]),
            vec![privacy]
        );
    }
}
