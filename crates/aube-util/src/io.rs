//! IO bridges between async chunk producers and blocking consumers.

/// Bridge from a tokio mpsc Receiver of byte chunks to a blocking
/// std::io::Read. Used by the streaming tarball pipeline to feed
/// HTTP body chunks into the gz+tar reader running on the blocking
/// pool. Each `Err` chunk surfaces as `Read::read` Err so the
/// downstream parser aborts cleanly.
pub struct ChunkReader {
    buffered: std::vec::IntoIter<bytes::Bytes>,
    rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>,
    current: bytes::Bytes,
    pos: usize,
}

impl ChunkReader {
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>) -> Self {
        Self::with_buffered(Vec::new(), rx)
    }

    /// Read `buffered` first, then continue with whatever arrives on `rx`.
    pub fn with_buffered(
        buffered: Vec<bytes::Bytes>,
        rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>,
    ) -> Self {
        Self {
            buffered: buffered.into_iter(),
            rx,
            current: bytes::Bytes::new(),
            pos: 0,
        }
    }
}

impl std::io::Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.current.len() {
                let n = (self.current.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if let Some(chunk) = self.buffered.next() {
                self.current = chunk;
                self.pos = 0;
                continue;
            }
            match self.rx.blocking_recv() {
                Some(Ok(chunk)) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Some(Err(e)) => return Err(e),
                None => return Ok(0),
            }
        }
    }
}
