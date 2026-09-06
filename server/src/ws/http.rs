//! The small amount of plain HTTP the protocol needs.
//!
//! The same listener serves the WebSocket endpoint and the OTA upload
//! endpoint, so a connection's request head is read before deciding which it
//! is. [`Prefixed`] then replays those already-consumed bytes to the WebSocket
//! handshake, which expects to read the request itself.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite};

/// The largest request head accepted, which bounds what an unauthenticated
/// client can make the server buffer.
const MAX_HEAD_LEN: usize = 16 * 1024;

/// A parsed HTTP request head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    /// Bytes of the body that arrived with the head.
    pub body_prefix: Vec<u8>,
}

impl RequestHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether this is a WebSocket upgrade request.
    pub fn is_websocket_upgrade(&self) -> bool {
        self.method.eq_ignore_ascii_case("GET")
            && self
                .header("upgrade")
                .map(|value| value.eq_ignore_ascii_case("websocket"))
                .unwrap_or(false)
    }

    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")?.trim().parse().ok()
    }
}

/// Parse a request head from `raw`, which must contain the terminating blank
/// line.
pub fn parse_head(raw: &[u8]) -> Option<RequestHead> {
    let separator = find_head_end(raw)?;
    let head = std::str::from_utf8(&raw[..separator]).ok()?;
    let mut lines = head.split("\r\n");

    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();

    let headers = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();

    Some(RequestHead {
        method,
        path,
        headers,
        body_prefix: raw[separator + 4..].to_vec(),
    })
}

fn find_head_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Read from `stream` until the request head is complete.
pub async fn read_head<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<(RequestHead, Vec<u8>)> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before the request head was complete",
            ));
        }
        raw.extend_from_slice(&chunk[..read]);
        if find_head_end(&raw).is_some() {
            break;
        }
        if raw.len() > MAX_HEAD_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
    }
    let head = parse_head(&raw).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed request head")
    })?;
    Ok((head, raw))
}

/// Render a JSON HTTP response.
pub fn json_response(status: u16, reason: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        body
    )
    .into_bytes()
}

/// A 405 carries the methods the endpoint does accept.
pub fn method_not_allowed(allow: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 405 Method Not Allowed\r\nAllow: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        allow
    )
    .into_bytes()
}

/// A stream that yields buffered bytes before reading from the inner stream.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    position: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            position: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.position < self.prefix.len() {
            let remaining = &self.prefix[self.position..];
            let len = remaining.len().min(buf.len());
            buf[..len].copy_from_slice(&remaining[..len]);
            self.position += len;
            return Poll::Ready(Ok(len));
        }
        Pin::new(&mut self.inner).poll_read(context, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(context, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::future::block_on;

    #[test]
    fn parses_a_websocket_upgrade() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: localhost:5580\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
        let head = parse_head(raw).unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.path, "/ws");
        assert!(head.is_websocket_upgrade());
        assert_eq!(head.header("host"), Some("localhost:5580"));
        assert!(head.body_prefix.is_empty());
    }

    #[test]
    fn parses_a_post_with_a_body_prefix() {
        let raw = b"POST /ota-upload/abc HTTP/1.1\r\nContent-Length: 4\r\n\r\nBODY";
        let head = parse_head(raw).unwrap();
        assert_eq!(head.method, "POST");
        assert_eq!(head.path, "/ota-upload/abc");
        assert_eq!(head.content_length(), Some(4));
        assert_eq!(head.body_prefix, b"BODY");
        assert!(!head.is_websocket_upgrade());
    }

    #[test]
    fn an_incomplete_head_does_not_parse() {
        assert!(parse_head(b"GET /ws HTTP/1.1\r\nHost: x\r\n").is_none());
    }

    #[test]
    fn header_lookup_ignores_case() {
        let raw = b"GET / HTTP/1.1\r\nUPGRADE: WebSocket\r\n\r\n";
        let head = parse_head(raw).unwrap();
        assert!(head.is_websocket_upgrade());
    }

    #[test]
    fn buffered_bytes_are_replayed_before_the_stream() {
        let inner = futures_lite::io::Cursor::new(b"world".to_vec());
        let mut stream = Prefixed::new(b"hello ".to_vec(), inner);
        let mut out = Vec::new();
        block_on(futures_lite::io::AsyncReadExt::read_to_end(
            &mut stream,
            &mut out,
        ))
        .unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn responses_carry_their_content_length() {
        let response = String::from_utf8(json_response(200, "OK", "{\"a\":1}")).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Length: 7\r\n"));
        assert!(response.ends_with("\r\n\r\n{\"a\":1}"));

        let denied = String::from_utf8(method_not_allowed("POST")).unwrap();
        assert!(denied.contains("Allow: POST\r\n"));
    }
}
