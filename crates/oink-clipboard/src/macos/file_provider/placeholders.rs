//! Bounded, one-shot typed namespaces; the five empty spares remain independent.
use super::*;
use crate::provider_preparation::{PlaceholderFallback, PlaceholderPoolStatus, PlaceholderTiming};
use std::collections::VecDeque;
mod worker;
pub(super) use worker::{
    Foreground, activation_current, complete, ledger_keys, protected, recover, retire, retry_retirements, start, take,
};
pub use worker::{configure_placeholder_pool, placeholder_pool_status, placeholder_status};

pub(super) const PARENTS: usize = 2;
pub(super) const FILES: u32 = 5;
pub(super) const FOLDERS: u32 = 5;

#[derive(Debug)]
pub(super) struct Ready {
    pub generation: u64,
    pub key: String,
    pub preparation_us: u64,
}
/// A consumed identity is retired even if cancellation wins before publication.
pub(super) struct Taken {
    pub key: String,
    pub ready: Option<Ready>,
    pub setting_epoch: u64,
    pub timing: PlaceholderTiming,
}
impl Taken {
    pub fn generation(&self) -> u64 {
        self.ready.as_ref().expect("unpublished stock").generation
    }
}
impl Drop for Taken {
    fn drop(&mut self) {
        if let Some(ready) = self.ready.take() {
            worker::retire(vec![ready.key]);
        }
        worker::release_taken(&self.key);
    }
}
#[derive(Default)]
pub(super) struct Pool {
    enabled: bool,
    background_allowed: bool,
    generation: u64,
    worker: bool,
    preparing: Option<String>,
    ready: VecDeque<Ready>,
    checked_out: Vec<String>,
    failure: Option<FileProviderReason>,
}
impl Pool {
    pub const fn new() -> Self {
        Self {
            enabled: false,
            background_allowed: false,
            generation: 0,
            worker: false,
            preparing: None,
            ready: VecDeque::new(),
            checked_out: Vec::new(),
            failure: None,
        }
    }
    pub(super) fn configure(&mut self, enabled: bool, background_allowed: bool) -> Vec<String> {
        let background_allowed = enabled && background_allowed;
        if self.enabled == enabled && self.background_allowed == background_allowed {
            return Vec::new();
        }
        self.generation = self.generation.wrapping_add(1);
        self.enabled = enabled;
        self.background_allowed = background_allowed;
        let mut retired = Vec::new();
        if !enabled {
            retired.extend(self.ready.drain(..).map(|spare| spare.key));
            self.failure = None;
        }
        if !background_allowed {
            retired.extend(self.preparing.iter().cloned());
        }
        retired
    }
    pub(super) fn start(&mut self) -> Option<u64> {
        if !self.enabled || !self.background_allowed || self.worker || self.ready.len() == PARENTS {
            return None;
        }
        self.worker = true;
        Some(self.generation)
    }
    pub(super) fn current(&self, generation: u64) -> bool {
        self.enabled && self.background_allowed && self.worker && self.generation == generation
    }
    pub(super) fn preparing(&mut self, generation: u64, key: String) -> bool {
        if !self.current(generation) || self.preparing.is_some() {
            return false;
        }
        self.preparing = Some(key);
        true
    }
    pub(super) fn finish(&mut self, generation: u64, result: Result<Ready, FileProviderReason>) -> bool {
        if !self.current(generation) {
            self.preparing = None;
            self.worker = false;
            return false;
        }
        self.preparing = None;
        match result {
            Ok(spare) => {
                self.ready.push_back(spare);
                self.failure = None;
                self.worker = self.ready.len() < PARENTS;
            }
            Err(reason) => {
                // Becoming busy is ordinary scheduling, not a broken provider.
                self.failure = (reason != FileProviderReason::Cancelled).then_some(reason);
                self.worker = false;
            }
        }
        self.worker
    }
    pub(super) fn protected(&self) -> impl Iterator<Item = &str> {
        self.preparing
            .iter()
            .map(String::as_str)
            .chain(self.ready.iter().map(|spare| spare.key.as_str()))
            .chain(self.checked_out.iter().map(String::as_str))
    }
    pub(super) fn status(&self, cleanup_pending: u32) -> PlaceholderPoolStatus {
        PlaceholderPoolStatus {
            enabled: self.enabled,
            ready_parents: self.ready.len() as u32,
            files_per_parent: FILES,
            folders_per_parent: FOLDERS,
            preparing: self.preparing.is_some(),
            last_background_failure: self.failure,
            cleanup_pending,
        }
    }
    pub(super) fn observe(&self, timing: &mut PlaceholderTiming) {
        *timing = PlaceholderTiming {
            enabled: self.enabled,
            ready_parents: self.ready.len() as u32,
            preparing: self.preparing.is_some(),
            failure: self.failure,
            fallback: if !self.enabled {
                PlaceholderFallback::Off
            } else if self.failure.is_some() {
                PlaceholderFallback::PreparationFailed
            } else {
                PlaceholderFallback::NotReady
            },
            ..Default::default()
        };
    }
    pub(super) fn checkout(
        &mut self,
        files: u32,
        folders: u32,
        known: bool,
        timing: &mut PlaceholderTiming,
    ) -> Option<Ready> {
        let ready = self.take(files, folders, known, timing)?;
        self.checked_out.push(ready.key.clone());
        Some(ready)
    }
    pub(super) fn release_checkout(&mut self, key: &str) {
        self.checked_out.retain(|k| k != key);
    }
    pub(super) fn take(
        &mut self,
        files: u32,
        folders: u32,
        known: bool,
        timing: &mut PlaceholderTiming,
    ) -> Option<Ready> {
        self.observe(timing);
        if !self.enabled {
            return None;
        }
        if files > FILES || folders > FOLDERS {
            timing.fallback = PlaceholderFallback::Capacity;
            return None;
        }
        if !known {
            timing.fallback = PlaceholderFallback::UnknownSize;
            return None;
        }
        let spare = self.ready.pop_front()?;
        timing.used = true;
        timing.prepared_files = FILES;
        timing.prepared_folders = FOLDERS;
        timing.used_files = files;
        timing.used_folders = folders;
        timing.preparation_us = Some(spare.preparation_us);
        timing.fallback = PlaceholderFallback::None;
        Some(spare)
    }
}

fn parent_name(key: &str) -> String {
    format!(".Oink-prepared-{key}.noindex")
}
pub(super) fn hide_parent(key: &str, catalog: &mut Catalog) {
    let parent = &mut catalog.records[0];
    parent.name = parent_name(key);
    parent.hidden = Some(true);
}
fn slot_id(key: &str, directory: bool, index: u32) -> String {
    format!("{key}-slot-{}-{index}", if directory { "folder" } else { "file" })
}
fn fill_catalog(key: &str, catalog: &mut Catalog) {
    hide_parent(key, catalog);
    catalog.records[0].children = Some(u64::from(FILES + FOLDERS));
    for (directory, count) in [(false, FILES), (true, FOLDERS)] {
        for index in 0..count {
            catalog.records.push(Record {
                id: slot_id(key, directory, index),
                parent: format!("{key}-root"),
                name: format!(".Oink-slot-{}-{index}", if directory { "folder" } else { "file" }),
                directory,
                size: None,
                modified: None,
                // Prepared catalogs have no bridge registration. Swift rejects
                // these hidden synthetic child requests before opening a socket;
                // the capability is rotated again when real nodes are attached.
                node: Some(u64::MAX - u64::from(index + if directory { FILES } else { 0 })),
                version: 1,
                children: None,
                hidden: Some(true),
            });
        }
    }
}
pub(super) fn bind(key: &str, state: &mut CatalogState) -> io::Result<()> {
    let mut files = 0;
    let mut folders = 0;
    for record in state.catalog.records.iter_mut().skip(1) {
        let index = if record.directory { &mut folders } else { &mut files };
        let capacity = if record.directory { FOLDERS } else { FILES };
        if *index >= capacity || (!record.directory && record.size.is_none()) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        record.id = slot_id(key, record.directory, *index);
        *index += 1;
        let node = record.node.ok_or(io::ErrorKind::InvalidData)?;
        state.aliases.insert(NodeId(node), record.id.clone());
    }
    if !state.catalog.records[0].parent.starts_with("source") {
        hide_parent(key, &mut state.catalog);
    }
    Ok(())
}
pub(super) fn restore_bindings(key: &str, previous: &Catalog, state: &mut CatalogState) -> io::Result<()> {
    let root_id = format!("{key}-root");
    let Some(root) = previous.records.iter().find(|record| record.id == root_id) else {
        return Ok(());
    };
    if !previous
        .records
        .iter()
        .any(|r| r.id.starts_with(&format!("{key}-slot-")))
    {
        return Ok(());
    }
    if root.hidden == Some(true) && root.name != parent_name(key) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut used = std::collections::HashSet::new();
    for old in previous.records.iter().filter(|record| record.parent == root_id) {
        let capacity = if old.directory { FOLDERS } else { FILES };
        if old.hidden.is_some()
            || !used.insert(old.id.clone())
            || !(0..capacity).any(|index| old.id == slot_id(key, old.directory, index))
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let new = state
            .catalog
            .records
            .iter_mut()
            .find(|record| record.parent == root_id && record.node == old.node && record.directory == old.directory)
            .ok_or(io::ErrorKind::InvalidData)?;
        new.id = old.id.clone();
        state
            .aliases
            .insert(NodeId(new.node.ok_or(io::ErrorKind::InvalidData)?), new.id.clone());
    }
    if root.hidden == Some(true) {
        hide_parent(key, &mut state.catalog);
    }
    Ok(())
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    fn ready(pool: &mut Pool, generation: u64, key: &str) {
        assert!(pool.preparing(generation, key.into()));
        pool.finish(
            generation,
            Ok(Ready {
                generation: 1,
                key: key.into(),
                preparation_us: 123,
            }),
        );
    }
    #[test]
    fn checked_out_parent_remains_protected_during_unlocked_os_wait_and_off() {
        let mut pool = Pool::new();
        pool.configure(true, true);
        let generation = pool.start().unwrap();
        ready(&mut pool, generation, "active");
        let taken = pool.checkout(1, 0, true, &mut Default::default()).unwrap();
        assert_eq!(pool.status(0).ready_parents, 0);
        assert_eq!(pool.protected().collect::<Vec<_>>(), ["active"]);
        // Off may revoke the in-flight publication through its setting epoch,
        // but only that publisher removes its identity after the helper stops.
        assert!(pool.configure(false, false).is_empty());
        assert_eq!(pool.protected().collect::<Vec<_>>(), ["active"]);
        pool.release_checkout(&taken.key);
        assert_eq!(pool.protected().count(), 0);
    }

    #[test]
    fn simultaneous_enters_cannot_take_the_same_typed_generation() {
        let mut pool = Pool::new();
        pool.configure(true, true);
        let generation = pool.start().unwrap();
        ready(&mut pool, generation, "one");
        let pool = Arc::new(Mutex::new(pool));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let pool = pool.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    pool.lock()
                        .unwrap()
                        .checkout(1, 0, true, &mut Default::default())
                        .map(|r| r.generation)
                })
            })
            .collect();
        barrier.wait();
        let taken: Vec<_> = workers.into_iter().filter_map(|w| w.join().unwrap()).collect();
        assert_eq!(taken, [1]);
        assert_eq!(pool.lock().unwrap().checked_out, ["one"]);
    }

    #[test]
    fn live_status_tracks_inventory_separately_from_drag_timing() {
        let mut pool = Pool::new();
        let status = pool.status(0);
        assert!(!status.enabled);
        assert_eq!(
            (status.ready_parents, status.files_per_parent, status.folders_per_parent),
            (0, 5, 5)
        );
        pool.configure(true, true);
        let generation = pool.start().unwrap();
        ready(&mut pool, generation, "first");
        assert!(pool.preparing(generation, "second".into()));
        let status = pool.status(2);
        assert!(status.enabled && status.preparing);
        assert_eq!((status.ready_parents, status.cleanup_pending), (1, 2));
        let mut timing = PlaceholderTiming::default();
        assert!(pool.take(1, 0, true, &mut timing).is_some());
        assert_eq!(timing.ready_parents, 1);
        assert_eq!(pool.status(0).ready_parents, 0);
        pool.finish(generation, Err(FileProviderReason::RegistrationFailed));
        assert_eq!(
            pool.status(0).last_background_failure,
            Some(FileProviderReason::RegistrationFailed)
        );
        assert!(!pool.status(0).preparing);
        pool.configure(false, false);
        assert_eq!(pool.status(0).last_background_failure, None);
        assert!(!pool.status(0).enabled);
    }
    #[test]
    fn off_is_default_and_never_starts_background_work() {
        let mut pool = Pool::new();
        assert!(pool.start().is_none());
        let mut timing = PlaceholderTiming::default();
        assert!(pool.take(1, 0, true, &mut timing).is_none());
        assert!(!timing.enabled);
        assert_eq!(timing.fallback, PlaceholderFallback::Off);
    }
    #[test]
    fn disabled_observations_leave_no_preparation_or_placeholder_identity() {
        let mut pool = Pool::new();
        let mut timing = PlaceholderTiming::default();
        // Domain/focus/pause wakes must not prepare placeholder stock when the
        // local setting is absent/off. Foreground keeps its normal spare path.
        for idle in [false, true, true, false, true] {
            assert!(pool.configure(false, idle).is_empty());
            assert!(pool.start().is_none());
            assert!(pool.take(2, 1, true, &mut timing).is_none());
            assert_eq!(timing, PlaceholderTiming::default());
            assert_eq!(pool.protected().count(), 0);
            assert!(!pool.worker);
        }
    }
    #[test]
    fn on_to_off_retires_every_unused_identity_and_late_worker_cannot_leave_one() {
        let mut pool = Pool::new();
        pool.configure(true, true);
        let generation = pool.start().unwrap();
        ready(&mut pool, generation, "ready");
        assert!(pool.preparing(generation, "pending".into()));
        let retired = pool.configure(false, false);
        assert_eq!(retired, ["ready", "pending"]);
        assert!(!pool.current(generation));
        assert!(pool.take(1, 0, true, &mut PlaceholderTiming::default()).is_none());
        assert!(!pool.finish(
            generation,
            Ok(Ready {
                generation: 1,
                key: "pending".into(),
                preparation_us: 1
            })
        ));
        assert_eq!(pool.protected().count(), 0);
        assert!(pool.configure(false, true).is_empty());
        assert!(pool.start().is_none());
        assert!(!pool.worker);
        // The returned identities are the durable asynchronous cleanup input;
        // native deletion completion is separately reported by cleanup_pending.
    }
    #[test]
    fn idle_only_bounded_pool_and_single_use_identity() {
        let mut pool = Pool::new();
        pool.configure(true, false);
        assert!(pool.start().is_none());
        pool.configure(true, true);
        let generation = pool.start().expect("enabled idle worker");
        assert!(pool.start().is_none());
        ready(&mut pool, generation, "one");
        ready(&mut pool, generation, "two");
        assert!(pool.start().is_none());
        let mut timing = PlaceholderTiming::default();
        assert_eq!(pool.take(2, 1, true, &mut timing).unwrap().key, "one");
        assert!(timing.enabled && timing.used);
        assert_eq!((timing.prepared_files, timing.prepared_folders), (5, 5));
        assert_eq!((timing.used_files, timing.used_folders), (2, 1));
        assert_eq!(timing.fallback, PlaceholderFallback::None);
        assert_eq!(pool.take(5, 5, true, &mut timing).unwrap().key, "two");
        assert!(pool.take(1, 0, true, &mut timing).is_none());
        assert_eq!(timing.fallback, PlaceholderFallback::NotReady);
    }
    #[test]
    fn unservable_batch_does_not_consume_a_ready_parent() {
        let mut pool = Pool::new();
        pool.configure(true, true);
        let generation = pool.start().unwrap();
        ready(&mut pool, generation, "one");
        let mut timing = PlaceholderTiming::default();
        for (files, folders, known, reason) in [
            (6, 0, true, PlaceholderFallback::Capacity),
            (0, 6, true, PlaceholderFallback::Capacity),
            (1, 0, false, PlaceholderFallback::UnknownSize),
        ] {
            assert!(pool.take(files, folders, known, &mut timing).is_none());
            assert_eq!(timing.fallback, reason);
        }
        assert_eq!(pool.take(1, 1, true, &mut timing).unwrap().key, "one");
    }
    #[test]
    fn disable_retires_ready_and_inflight_and_never_accepts_late_completion() {
        let mut pool = Pool::new();
        pool.configure(true, true);
        let old = pool.start().unwrap();
        ready(&mut pool, old, "ready");
        assert!(pool.preparing(old, "inflight".into()));
        let mut retired = pool.configure(false, false);
        retired.sort();
        assert_eq!(retired, ["inflight", "ready"]);
        assert_eq!(
            pool.protected().collect::<Vec<_>>(),
            ["inflight"],
            "cleanup must wait until the old writer and helper stop"
        );
        assert!(!pool.finish(
            old,
            Ok(Ready {
                generation: 1,
                key: "inflight".into(),
                preparation_us: 1
            })
        ));
        assert!(pool.start().is_none());
        pool.configure(true, true);
        let new = pool.start().unwrap();
        assert_ne!(old, new);
        assert!(!pool.preparing(old, "late".into()));
        ready(&mut pool, new, "fresh");
        let mut timing = PlaceholderTiming::default();
        assert_eq!(pool.take(1, 0, true, &mut timing).unwrap().key, "fresh");
    }
    #[test]
    fn becoming_busy_cancels_preparation_but_preserves_ready_for_foreground() {
        let mut pool = Pool::new();
        pool.configure(true, true);
        let generation = pool.start().unwrap();
        ready(&mut pool, generation, "ready");
        assert!(pool.preparing(generation, "inflight".into()));
        assert_eq!(pool.configure(true, false), ["inflight"]);
        assert!(!pool.finish(
            generation,
            Ok(Ready {
                generation: 1,
                key: "inflight".into(),
                preparation_us: 1
            })
        ));
        assert!(pool.start().is_none());
        let mut timing = PlaceholderTiming::default();
        assert_eq!(pool.take(1, 0, true, &mut timing).unwrap().key, "ready");
    }
    #[test]
    fn private_slots_are_typed_hidden_and_have_no_remote_content_capability() {
        let (batch, _) = prepare_batch(
            Flat::new(&[], Arc::new(EmptyContents)).unwrap(),
            "batch",
            "secret".into(),
            1,
            PathBuf::new(),
        )
        .unwrap();
        let mut state = batch.state.lock().unwrap();
        fill_catalog("batch", &mut state.catalog);
        assert_eq!(state.catalog.records.len(), 11);
        assert_eq!(state.catalog.records[0].name, parent_name("batch"));
        assert!(state.catalog.records.iter().all(|r| r.hidden == Some(true)));
        assert_eq!(
            state
                .catalog
                .records
                .iter()
                .filter(|r| r.node.is_some() && !r.directory)
                .count(),
            5
        );
        assert_eq!(
            state
                .catalog
                .records
                .iter()
                .filter(|r| r.node.is_some() && r.directory)
                .count(),
            5
        );
        assert!(state.catalog.records.iter().all(|r| r.size.is_none()));
        assert!(batch.contents.lock().unwrap().open(NodeId(1), 1, 0).is_err());
    }
    #[test]
    fn binding_preserves_root_order_and_maps_nested_parent_without_converting_kind() {
        let entries = [
            FileEntry {
                path: "folder".into(),
                is_dir: true,
                size: 0,
                modified_ms: None,
                local_path: None,
            },
            FileEntry {
                path: "file.txt".into(),
                is_dir: false,
                size: 4,
                modified_ms: None,
                local_path: None,
            },
        ];
        let (batch, _) = prepare_batch(
            Flat::new(&entries, Arc::new(EmptyContents)).unwrap(),
            "batch",
            "secret".into(),
            1,
            PathBuf::new(),
        )
        .unwrap();
        let mut state = batch.state.lock().unwrap();
        bind("batch", &mut state).unwrap();
        assert_eq!(state.catalog.records[1].id, "batch-slot-folder-0");
        assert_eq!(state.catalog.records[2].id, "batch-slot-file-0");
        assert_eq!(state.catalog.records[1].name, "folder");
        assert_eq!(state.catalog.records[2].size, Some(4));
        assert!(state.catalog.records[1..].iter().all(|r| r.hidden.is_none()));
        let child = FileNode {
            id: NodeId(73),
            parent: Some(NodeId(1)),
            name: "nested.txt".into(),
            kind: NodeKind::File,
            size: ContentSize::Known(3),
            modified_ms: None,
            version: 1,
        };
        let CatalogState { names, aliases, .. } = &mut *state;
        let record = node_record_with_aliases("batch", &child, names, aliases).unwrap();
        assert_eq!(record.parent, "batch-slot-folder-0");
        assert_eq!(record.id, "batch-73");
    }
    #[test]
    fn received_typed_batch_exposes_timestamp_and_restores_slot_identity() {
        let entries = [FileEntry {
            path: "file.txt".into(),
            is_dir: false,
            size: 4,
            modified_ms: None,
            local_path: None,
        }];
        let make = || {
            prepare_batch(
                Flat::new(&entries, Arc::new(EmptyContents)).unwrap(),
                "batch",
                "token".into(),
                1,
                PathBuf::new(),
            )
            .unwrap()
            .0
        };
        let batch = make();
        let mut state = batch.state.lock().unwrap();
        state.catalog.records[0].parent = "source0123".into();
        state.catalog.records[0].name = "20261021-234512-123".into();
        bind("batch", &mut state).unwrap();
        assert_eq!(state.catalog.records[0].name, "20261021-234512-123");
        assert!(state.catalog.records[0].hidden.is_none());
        let previous = state.catalog.clone();
        drop(state);
        let recovered = make();
        let mut state = recovered.state.lock().unwrap();
        restore_bindings("batch", &previous, &mut state).unwrap();
        assert_eq!(state.catalog.records[1].id, "batch-slot-file-0");
    }
    #[test]
    fn completed_restart_preserves_slot_urls_and_remote_node_identity() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let owned = crate::content::Completed::test_single_file(data.path());
        let local = crate::content::Completed::restore(data.path(), owned.id()).unwrap();
        let path = root.path().join("batch.json");
        let (batch, _) = prepare_batch(Arc::new(local), "batch", "old".into(), 1, path.clone()).unwrap();
        let original;
        {
            let mut state = batch.state.lock().unwrap();
            bind("batch", &mut state).unwrap();
            original = state.catalog.records[0].clone();
            state.catalog.urls = vec![root.path().join("final.txt")];
            save_catalog(&path, &state.catalog).unwrap();
        }
        drop(batch);
        let restored = restore_catalog(data.path(), &path, "batch", 2).unwrap();
        let state = restored.state.lock().unwrap();
        assert_eq!(state.catalog.records[1].id, "batch-slot-file-0");
        assert_eq!(state.catalog.records[1].node, Some(1));
        assert_eq!(state.catalog.records[0].name, original.name);
        assert_eq!(state.catalog.records[0].parent, original.parent);
        assert!(state.catalog.records[0].parent.starts_with("source"));
        assert!(state.catalog.records[0].hidden.is_none());
    }
}
