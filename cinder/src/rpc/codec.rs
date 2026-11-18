use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum LineRead {
    Line(Vec<u8>),
    Oversized,
}

pub fn is_blank(line: &[u8]) -> bool {
    line.iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | 0x0b | 0x0c))
}

pub struct LineReader<R> {
    inner: R,
    cap: usize,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    pub fn with_cap(inner: R, cap: usize) -> Self {
        Self { inner, cap }
    }

    /// One line per call. Lines longer than `cap` bytes before the newline are
    /// drained without buffering and reported as `Oversized`; `Ok(None)` is EOF.
    pub async fn read_line(&mut self) -> io::Result<Option<LineRead>> {
        let mut buf = Vec::new();
        let mut oversized = false;
        let mut len = 0usize;
        loop {
            let avail = self.inner.fill_buf().await?;
            if avail.is_empty() {
                if oversized {
                    return Ok(Some(LineRead::Oversized));
                }
                if len == 0 {
                    return Ok(None);
                }
                return Ok(Some(LineRead::Line(buf)));
            }
            let (segment, consumed, hit_newline) = match avail.iter().position(|b| *b == b'\n') {
                Some(i) => (&avail[..i], i + 1, true),
                None => (avail, avail.len(), false),
            };
            if !oversized {
                if len + segment.len() > self.cap {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(segment);
                    len += segment.len();
                }
            }
            self.inner.consume(consumed);
            if hit_newline {
                return Ok(Some(if oversized {
                    LineRead::Oversized
                } else {
                    LineRead::Line(buf)
                }));
            }
        }
    }
}

pub struct FrameWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// The only bytes ever emitted on the sink: one serialized frame plus '\n',
    /// flushed immediately (AGENT-API §1.3).
    pub async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.inner.write_all(frame).await?;
        self.inner.write_all(b"\n").await?;
        self.inner.flush().await
    }
}
