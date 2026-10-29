//! One shared byte budget covers completed and in-flight native formats. Readers
//! retain immutable blocks without copying them or owning any filesystem path.
use super::*;
use std::sync::atomic::AtomicU64;

pub(super) struct Budget {
    used: AtomicU64,
    limit: u64,
}
impl Budget {
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicU64::new(0),
            limit,
        })
    }
    fn reserve(&self, size: u64) -> io::Result<()> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(size).filter(|total| *total <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| unavailable(Unavailable::LimitExceeded))
    }
}

pub(super) struct Bytes {
    blocks: Vec<Box<[u8]>>,
    pub size: u64,
    budget: Arc<Budget>,
}
impl Bytes {
    pub fn new(budget: Arc<Budget>) -> Self {
        Self {
            blocks: Vec::new(),
            size: 0,
            budget,
        }
    }
    pub fn push(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        self.budget.reserve(bytes.len() as u64)?;
        self.size += bytes.len() as u64;
        self.blocks.push(bytes.into_boxed_slice());
        Ok(())
    }
    pub fn reader(self: &Arc<Self>) -> FileReader {
        Box::new(Reader {
            bytes: self.clone(),
            block: 0,
            offset: 0,
        })
    }
}
impl Drop for Bytes {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.size, Ordering::AcqRel);
    }
}
struct Reader {
    bytes: Arc<Bytes>,
    block: usize,
    offset: usize,
}
impl Read for Reader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        while let Some(bytes) = self.bytes.blocks.get(self.block) {
            let count = output.len().min(bytes.len() - self.offset);
            if count == 0 {
                self.block += 1;
                self.offset = 0;
                continue;
            }
            output[..count].copy_from_slice(&bytes[self.offset..self.offset + count]);
            self.offset += count;
            return Ok(count);
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_formats_and_retained_readers_share_one_budget() {
        let budget = Budget::new(8);
        let mut first = Bytes::new(budget.clone());
        first.push(vec![1; 5]).unwrap();
        let first = Arc::new(first);
        let reader = first.reader();
        drop(first);
        let mut second = Bytes::new(budget.clone());
        second.push(vec![2; 3]).unwrap();
        assert_eq!(reason(&second.push(vec![3]).unwrap_err()), Unavailable::LimitExceeded);
        drop(reader);
        second.push(vec![4; 5]).unwrap();
        drop(second);
        let mut retry = Bytes::new(budget);
        retry.push(vec![0; 8]).unwrap();
    }
    #[test]
    fn independent_readers_cross_blocks_and_empty_reads_preserve_position() {
        let mut bytes = Bytes::new(Budget::new(8));
        bytes.push(b"abc".to_vec()).unwrap();
        bytes.push(Vec::new()).unwrap();
        bytes.push(b"defgh".to_vec()).unwrap();
        let bytes = Arc::new(bytes);
        let mut first = bytes.reader();
        let mut second = bytes.reader();
        assert_eq!(first.read(&mut []).unwrap(), 0);
        let mut prefix = [0; 4];
        first.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"abcd");
        assert_eq!(read_all(&mut first).unwrap(), b"efgh");
        assert_eq!(read_all(&mut second).unwrap(), b"abcdefgh");
    }
}
