//! Byte I/O traits for the link TCP pool. Framing belongs to link::tcp.
use std::{
    io,
    task::{Context as TaskContext, Poll},
};
pub(crate) trait RecordWrite {
    async fn record(&mut self, header: &[u8], body: &[u8]) -> io::Result<()>;
    fn poll_write_bytes(&mut self, cx: &mut TaskContext<'_>, data: &[u8]) -> Poll<io::Result<usize>>;
    fn poll_flush_bytes(&mut self, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>>;
    async fn finish_records(&mut self) -> io::Result<()>;
    async fn flush_message(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(crate) trait RecordRead {
    fn finish_read(&mut self) {}
    async fn read_record_bytes(&mut self, data: &mut [u8]) -> io::Result<usize>;
}
