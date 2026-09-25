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

#[cfg(test)]
mod tests {
    use super::ChunkReader;
    use std::io::Read;

    fn chunk(text: &str) -> bytes::Bytes {
        bytes::Bytes::copy_from_slice(text.as_bytes())
    }

    #[test]
    fn reads_buffered_chunks_then_the_channel() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.try_send(Ok(chunk("three"))).unwrap();
        drop(tx);
        let mut reader = ChunkReader::with_buffered(vec![chunk("one "), chunk("two ")], rx);
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "one two three");
    }

    #[test]
    fn surfaces_a_channel_error_after_the_buffered_prefix() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.try_send(Err(std::io::Error::other("stream reset")))
            .unwrap();
        drop(tx);
        let mut reader = ChunkReader::with_buffered(vec![chunk("prefix")], rx);
        let mut out = [0u8; 6];
        reader.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"prefix");
        let err = reader.read(&mut [0u8; 8]).unwrap_err();
        assert_eq!(err.to_string(), "stream reset");
    }

    #[test]
    fn buffered_only_body_ends_when_the_channel_closes() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(tx);
        let mut reader = ChunkReader::with_buffered(vec![chunk("whole body")], rx);
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "whole body");
    }
}
