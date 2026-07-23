//! Content-Length codec (read + write). See `docs/design/dap-proxy.md` §3.1.

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};

/// Incremental DAP frame reader over any async byte stream.
pub struct DapReader<R: AsyncRead + Unpin> {
    inner: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> DapReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
            buf: Vec::new(),
        }
    }

    /// Reads exactly one message body (raw JSON bytes). Handles multiple
    /// messages per read() and split headers. Header: `Content-Length: N\r\n\r\n`
    /// (tolerates extra headers before the blank line). Returns `Ok(None)` on EOF.
    ///
    /// Cancel-safe: bytes already consumed from the stream live in `self.buf`
    /// and survive a dropped call (the proxy `select!`s on this future).
    pub async fn next_message(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some((body_start, body_len)) = parse_header(&self.buf)? {
                let total = body_start
                    .checked_add(body_len)
                    .with_context(|| format!("Content-Length {body_len} too large"))?;
                if self.buf.len() >= total {
                    let body = self.buf[body_start..total].to_vec();
                    self.buf.drain(..total);
                    return Ok(Some(body));
                }
            }
            let mut chunk = [0u8; 8192];
            let n = self
                .inner
                .read(&mut chunk)
                .await
                .context("reading DAP stream")?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                anyhow::bail!(
                    "DAP stream closed mid-frame ({} buffered bytes)",
                    self.buf.len()
                );
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// If `buf` starts with a complete header block, returns
/// `Some((body_start, content_length))`. `None` = need more bytes.
/// `Err` = complete but malformed header block.
fn parse_header(buf: &[u8]) -> Result<Option<(usize, usize)>> {
    let Some(end) = find_subslice(buf, b"\r\n\r\n") else {
        return Ok(None);
    };
    let header = std::str::from_utf8(&buf[..end]).context("non-UTF-8 DAP header")?;
    let mut len: Option<usize> = None;
    for line in header.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                len = Some(
                    value
                        .trim()
                        .parse()
                        .with_context(|| format!("invalid Content-Length in {header:?}"))?,
                );
            }
            // Other headers (spec allows them) are tolerated and ignored.
        }
    }
    match len {
        Some(l) => Ok(Some((end + 4, l))),
        None => anyhow::bail!("DAP header block without Content-Length: {header:?}"),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Frames a message body: `"Content-Length: {n}\r\n\r\n"` + body.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    /// Deterministic AsyncRead: yields exactly one predefined chunk per read
    /// call, then EOF. Lets tests force header/body splits at any byte.
    struct ChunkReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl ChunkReader {
        fn new<I: IntoIterator<Item = Vec<u8>>>(chunks: I) -> Self {
            Self {
                chunks: chunks.into_iter().collect(),
            }
        }
    }

    impl AsyncRead for ChunkReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if let Some(chunk) = self.chunks.pop_front() {
                buf.put_slice(&chunk);
            }
            Poll::Ready(Ok(()))
        }
    }

    fn reader_of(chunks: Vec<Vec<u8>>) -> DapReader<ChunkReader> {
        DapReader::new(ChunkReader::new(chunks))
    }

    #[test]
    fn frame_exact_bytes() {
        assert_eq!(frame(b"{}"), b"Content-Length: 2\r\n\r\n{}".to_vec());
    }

    #[tokio::test]
    async fn single_message_roundtrip() {
        let body = br#"{"seq":1,"type":"request","command":"initialize"}"#;
        let mut r = reader_of(vec![frame(body)]);
        assert_eq!(r.next_message().await.unwrap().unwrap(), body.to_vec());
        assert!(r.next_message().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn two_messages_in_one_read() {
        let mut joined = frame(b"{\"a\":1}");
        joined.extend_from_slice(&frame(b"{\"b\":2}"));
        let mut r = reader_of(vec![joined]);
        assert_eq!(r.next_message().await.unwrap().unwrap(), b"{\"a\":1}");
        assert_eq!(r.next_message().await.unwrap().unwrap(), b"{\"b\":2}");
        assert!(r.next_message().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn header_and_body_split_across_reads() {
        // Split inside the header name, inside the \r\n\r\n, and inside the body.
        let chunks = vec![
            b"Content-Le".to_vec(),
            b"ngth: 8\r\n".to_vec(),
            b"\r".to_vec(),
            b"\n{\"x\"".to_vec(),
            b":42}".to_vec(),
        ];
        let mut r = reader_of(chunks);
        assert_eq!(r.next_message().await.unwrap().unwrap(), b"{\"x\":42}");
        assert!(r.next_message().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn second_message_split_after_first() {
        // First frame complete + second frame's header start in one read.
        let mut first = frame(b"{\"a\":1}");
        first.extend_from_slice(b"Content-Length");
        let chunks = vec![first, b": 7\r\n\r\n{\"b\":2}".to_vec()];
        let mut r = reader_of(chunks);
        assert_eq!(r.next_message().await.unwrap().unwrap(), b"{\"a\":1}");
        assert_eq!(r.next_message().await.unwrap().unwrap(), b"{\"b\":2}");
    }

    #[tokio::test]
    async fn extra_headers_tolerated() {
        let raw =
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 4\r\n\r\nnull"
                .to_vec();
        let mut r = reader_of(vec![raw]);
        assert_eq!(r.next_message().await.unwrap().unwrap(), b"null");
    }

    #[tokio::test]
    async fn extra_headers_after_content_length_tolerated() {
        let raw = b"Content-Length: 4\r\nX-Custom: zed\r\n\r\ntrue".to_vec();
        let mut r = reader_of(vec![raw]);
        assert_eq!(r.next_message().await.unwrap().unwrap(), b"true");
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let mut r = reader_of(vec![]);
        assert!(r.next_message().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eof_mid_frame_is_error() {
        let mut full = frame(b"{\"a\":1}");
        full.truncate(full.len() - 3); // chop the body
        let mut r = reader_of(vec![full]);
        assert!(r.next_message().await.is_err());
    }

    #[tokio::test]
    async fn missing_content_length_is_error() {
        let mut r = reader_of(vec![b"X-Only: 1\r\n\r\n{}".to_vec()]);
        assert!(r.next_message().await.is_err());
    }

    #[tokio::test]
    async fn invalid_content_length_is_error() {
        let mut r = reader_of(vec![b"Content-Length: nope\r\n\r\n{}".to_vec()]);
        assert!(r.next_message().await.is_err());
    }

    #[tokio::test]
    async fn overflowing_content_length_is_error() {
        // A huge Content-Length must join the graceful malformed-header path,
        // not overflow `body_start + body_len` (panic in debug, wrapping slice
        // panic in release).
        let mut r = reader_of(vec![
            b"Content-Length: 18446744073709551615\r\n\r\n{}".to_vec()
        ]);
        assert!(r.next_message().await.is_err());
    }
}
