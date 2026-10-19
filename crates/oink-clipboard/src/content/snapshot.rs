use super::*;

/// Native-drop non-file snapshot. Each reader retains the same cache pin; clearing
/// a drag registration or receiving TargetStored cannot delete a reader's backing file.
pub struct Snapshot {
    inventory: Inventory,
    batch: Batch,
    translator: Arc<dyn Translator>,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("items", &self.inventory.items.len())
            .finish()
    }
}
impl Snapshot {
    pub(crate) fn from_parts(
        inventory: Inventory,
        batch: Batch,
        translator: Arc<dyn Translator>,
    ) -> io::Result<Arc<Self>> {
        inventory.validate().map_err(unavailable)?;
        batch.complete()?;
        Ok(Arc::new(Self {
            inventory,
            batch,
            translator,
        }))
    }
    pub fn capture(data: &Path, id: BatchId, provider: &dyn Provider) -> io::Result<Arc<Self>> {
        let mut inventory = provider.inventory();
        for rep in inventory.items.iter_mut().flat_map(|i| &mut i.representations) {
            if matches!(rep.data.size(), Some(ContentSize::Known(n)) if n > REPRESENTATION_BYTES) {
                rep.data = RepresentationData::Unavailable(Unavailable::LimitExceeded);
            }
        }
        inventory.validate().map_err(unavailable)?;
        let batch = Batch::create(data, StoreKind::SourceSnapshot, id)?;
        for rep in inventory.items.iter_mut().flat_map(|i| &mut i.representations) {
            let mut reader = match &rep.data {
                RepresentationData::Reader { .. } => match provider.open(rep.id) {
                    Ok(reader) => reader,
                    Err(error) => {
                        rep.data = RepresentationData::Unavailable(reason(&error));
                        continue;
                    }
                },
                RepresentationData::Inline(bytes) => Box::new(io::Cursor::new(bytes.clone())) as FileReader,
                RepresentationData::File(_) | RepresentationData::Promise(..) | RepresentationData::Unavailable(_) => {
                    continue;
                }
            };
            rep.data = match batch.snapshot(rep.id.0, &mut reader) {
                Ok((size, hash)) => RepresentationData::Reader {
                    size: ContentSize::Known(size),
                    hash: Some(hash),
                },
                Err(error) => RepresentationData::Unavailable(reason(&error)),
            };
        }
        // Inventory includes explicit per-representation failures. Nothing is
        // silently truncated and the surviving native order remains unchanged.
        inventory.validate().map_err(unavailable)?;
        Self::from_parts(inventory, batch, provider.translator())
    }
    pub fn batch(&self) -> &Batch {
        &self.batch
    }
}
impl Provider for Snapshot {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, representation: RepresentationId) -> io::Result<FileReader> {
        let rep = find(&self.inventory, representation).ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if let RepresentationData::Inline(bytes) = &rep.data {
            return Ok(Box::new(io::Cursor::new(bytes.clone())));
        }
        let RepresentationData::Reader { .. } = rep.data else {
            return Err(unavailable(match rep.data {
                RepresentationData::Unavailable(reason) => reason,
                _ => Unavailable::NoSuchItem,
            }));
        };
        let file = std::fs::File::open(self.batch.path().join(format!("r-{}", representation.0)))?;
        Ok(Box::new(PinnedReader {
            file,
            _batch: self.batch.clone(),
        }))
    }
    fn local_batch(&self) -> Option<Batch> {
        Some(self.batch.clone())
    }
    fn translator(&self) -> Arc<dyn Translator> {
        self.translator.clone()
    }
}
struct PinnedReader {
    file: std::fs::File,
    _batch: Batch,
}
impl Read for PinnedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}
