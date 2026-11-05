//! Newline-delimited request frames with a byte cap.
//!
//! Requests are read before their token is checked, so the reader must bound
//! what any connected peer can make the backend buffer. Every API request is a
//! small JSON document (ids, tokens, layout edits); 1 MiB leaves ample room.
use std::io;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

pub(crate) const MAX_FRAME: usize = 1 << 20;
/// Clients write their first request as soon as they connect.
pub(crate) const FIRST_REQUEST: Duration = Duration::from_secs(10);

pub(crate) struct Frames<R> {
    reader: BufReader<R>,
    line: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Frames<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            line: Vec::new(),
        }
    }

    /// Like `Lines::next_line`: strips `\n` / `\r\n` and returns a final
    /// unterminated line at EOF. A line longer than [`MAX_FRAME`] is an
    /// `InvalidData` error before it is buffered in full.
    pub(crate) async fn next_line(&mut self) -> io::Result<Option<String>> {
        self.line.clear();
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.line.is_empty() {
                    return Ok(None);
                }
                break;
            }
            let (chunk, end) = match available.iter().position(|&b| b == b'\n') {
                Some(at) => (at, true),
                None => (available.len(), false),
            };
            if self.line.len() + chunk > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "local IPC request exceeds the frame limit",
                ));
            }
            self.line.extend_from_slice(&available[..chunk]);
            self.reader.consume(chunk + usize::from(end));
            if end {
                break;
            }
        }
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        String::from_utf8(std::mem::take(&mut self.line))
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// The first request must arrive within [`FIRST_REQUEST`]; an idle
    /// connection ends as if the peer had closed it.
    pub(crate) async fn first_line(&mut self) -> io::Result<Option<String>> {
        tokio::time::timeout(FIRST_REQUEST, self.next_line())
            .await
            .unwrap_or(Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn lines_match_tokio_lines_within_the_limit() {
        let mut frames = Frames::new(&b"{\"a\":1}\r\nsecond\nlast"[..]);
        assert_eq!(frames.next_line().await.unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(frames.next_line().await.unwrap().as_deref(), Some("second"));
        assert_eq!(frames.next_line().await.unwrap().as_deref(), Some("last"));
        assert_eq!(frames.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_frame_of_exactly_the_limit_is_accepted() {
        let mut input = vec![b'x'; MAX_FRAME];
        input.push(b'\n');
        let mut frames = Frames::new(&input[..]);
        assert_eq!(
            frames.next_line().await.unwrap().map(|line| line.len()),
            Some(MAX_FRAME)
        );
    }

    #[tokio::test]
    async fn oversized_terminated_and_unterminated_frames_are_rejected() {
        let mut terminated = vec![b'x'; MAX_FRAME + 1];
        terminated.push(b'\n');
        let error = Frames::new(&terminated[..]).next_line().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        // An unauthenticated peer that never sends a newline is cut off at the
        // cap while the connection is still open, not after it closes.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let writer = tokio::spawn(async move {
            let chunk = vec![b'x'; 64 * 1024];
            while client.write_all(&chunk).await.is_ok() {}
        });
        let error = Frames::new(server).next_line().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_first_request_ends_the_connection() {
        let (_client, server) = tokio::io::duplex(1024);
        assert_eq!(Frames::new(server).first_line().await.unwrap(), None);
    }
}
