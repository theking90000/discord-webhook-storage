//! Check download ranges, response framing, and connection reuse boundaries.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use discord_webhook_storage::{
    DiscordFile, DiscordFileUrl, DiscordFileUrlError, Error, HttpError, ReadFile,
};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, executor::block_on};

struct Transport {
    response: Vec<u8>,
    read_at: usize,
    written: Vec<u8>,
    read_limit: usize,
    write_limit: usize,
    flushes: usize,
    closes: usize,
    reads: usize,
    pending: bool,
    yield_read: bool,
    yield_write: bool,
    yield_flush: bool,
    fail_read: bool,
    fail_read_at: Option<usize>,
    fail_write: bool,
    fail_flush: bool,
    fail_at_eof: bool,
}

impl Transport {
    fn new(response: impl Into<Vec<u8>>) -> Self {
        Self {
            response: response.into(),
            read_at: 0,
            written: Vec::new(),
            read_limit: usize::MAX,
            write_limit: usize::MAX,
            flushes: 0,
            closes: 0,
            reads: 0,
            pending: false,
            yield_read: false,
            yield_write: false,
            yield_flush: false,
            fail_read: false,
            fail_read_at: None,
            fail_write: false,
            fail_flush: false,
            fail_at_eof: false,
        }
    }
}

impl AsyncRead for Transport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.reads += 1;
        if self.fail_read || self.fail_read_at.is_some_and(|at| self.read_at >= at) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "read failed",
            )));
        }
        if self.flushes == 0 || !self.written.ends_with(b"\r\n\r\n") {
            return Poll::Ready(Err(io::Error::other("request was not flushed")));
        }
        if self.pending {
            self.yield_read = !self.yield_read;
            if self.yield_read {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        if self.fail_at_eof && self.read_at == self.response.len() {
            return Poll::Ready(Err(io::Error::other("read beyond response")));
        }
        let n = buf
            .len()
            .min(self.read_limit)
            .min(self.response.len() - self.read_at);
        buf[..n].copy_from_slice(&self.response[self.read_at..self.read_at + n]);
        self.read_at += n;
        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.fail_write {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "write failed",
            )));
        }
        if self.pending {
            self.yield_write = !self.yield_write;
            if self.yield_write {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        let n = buf.len().min(self.write_limit);
        self.written.extend_from_slice(&buf[..n]);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.fail_flush {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "flush failed",
            )));
        }
        if self.pending {
            self.yield_flush = !self.yield_flush;
            if self.yield_flush {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        self.flushes += 1;
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.closes += 1;
        Poll::Ready(Ok(()))
    }
}

fn url() -> DiscordFileUrl {
    DiscordFileUrl::parse(
        "https://cdn.discordapp.com/attachments/123/456/file.bin?ex=ffffffffffffffff&is=123&hm=aabbcc",
    )
    .unwrap()
}

fn response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[test]
fn open_accepts_files_urls_and_their_references() {
    block_on(async {
        let file = DiscordFile {
            id: "message".into(),
            url: url(),
        };
        let bytes = response("206 Partial Content", b"abc");
        let mut transport = Transport::new(bytes.clone());
        let mut reader = ReadFile::open(&mut transport, &file).await.unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"abc");
        drop(reader);
        assert_eq!(
            transport.written.as_slice(),
            b"GET /attachments/123/456/file.bin?ex=ffffffffffffffff&is=123&hm=aabbcc HTTP/1.1\r\n\
             Host: cdn.discordapp.com\r\n\
             Range: bytes=0-\r\n\
             Accept-Encoding: identity\r\n\r\n"
        );
        assert_eq!(transport.closes, 0);
        ReadFile::open(Transport::new(bytes.clone()), file)
            .await
            .unwrap();
        ReadFile::open(Transport::new(bytes.clone()), url())
            .await
            .unwrap();
        ReadFile::open(Transport::new(bytes), &url()).await.unwrap();
    });
}

#[test]
fn range_headers_use_inclusive_end_and_support_open_end() {
    block_on(async {
        for (end, range) in [(Some(7), "bytes=3-7"), (None, "bytes=3-")] {
            let mut transport = Transport::new(response("206 Partial Content", b"abcde"));
            let mut reader = ReadFile::open_with_range(&mut transport, url(), 3, end)
                .await
                .unwrap();
            let mut body = Vec::new();
            reader.read_to_end(&mut body).await.unwrap();
            assert_eq!(body, b"abcde");
            drop(reader);
            assert!(
                String::from_utf8(transport.written)
                    .unwrap()
                    .contains(&format!("\r\nRange: {range}\r\n"))
            );
        }
    });
}

#[test]
fn invalid_urls_and_ranges_fail_before_sending() {
    let mut expired = url();
    expired.ex = 0;
    let mut transport = Transport::new(Vec::new());
    assert!(matches!(
        block_on(ReadFile::open(&mut transport, expired)),
        Err(Error::InvalidDiscordFileUrl(DiscordFileUrlError::Expired))
    ));
    for (start, end) in [(4, 4), (5, 4)] {
        assert!(
            matches!(block_on(ReadFile::open_with_range(&mut transport, url(), start, Some(end))),
            Err(Error::InvalidRange { start: actual_start, end: actual_end })
                if actual_start == start && actual_end == end)
        );
    }
    let mut malformed = url();
    malformed.attachment_name = "file.bin\r\nX-Injected: yes".into();
    assert!(matches!(
        block_on(ReadFile::open(&mut transport, malformed)),
        Err(Error::InvalidDiscordFileUrl(_))
    ));
    assert!(transport.written.is_empty());
    assert_eq!(transport.reads, 0);
}

#[test]
fn fragmented_io_and_pending_preserve_body_and_eof() {
    block_on(async {
        for read_limit in [1, 7, usize::MAX] {
            let mut transport = Transport::new(response("206 Partial Content", b"abcdef"));
            transport.read_limit = read_limit;
            transport.write_limit = 3;
            transport.pending = true;
            transport.fail_at_eof = true;
            let mut reader = ReadFile::open(&mut transport, url()).await.unwrap();
            assert_eq!(reader.read(&mut []).await.unwrap(), 0);
            let mut body = Vec::new();
            let mut buf = [0; 2];
            loop {
                let n = reader.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n]);
            }
            assert_eq!(body, b"abcdef");
            assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
            drop(reader);
            assert_eq!(transport.read_at, transport.response.len());
            assert_eq!(transport.closes, 0);
        }
    });
}

#[test]
fn large_body_stops_before_next_response_and_releases_connection() {
    block_on(async {
        let body = vec![42; 8192];
        let mut bytes = response("206 Partial Content", &body);
        let first_end = bytes.len();
        bytes.extend_from_slice(&response("206 Partial Content", b"next"));
        let mut transport = Transport::new(bytes);
        let mut reader = ReadFile::open(&mut transport, url()).await.unwrap();
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).await.unwrap();
        assert_eq!(actual, body);
        drop(reader);
        assert_eq!(transport.read_at, first_end);
        let mut reader = ReadFile::open(&mut transport, url()).await.unwrap();
        actual.clear();
        reader.read_to_end(&mut actual).await.unwrap();
        assert_eq!(actual, b"next");
        drop(reader);
        assert_eq!(transport.closes, 0);
    });
}

#[test]
fn zero_length_body_returns_eof_without_reading_again() {
    block_on(async {
        let mut transport = Transport::new(response("200 OK", b""));
        transport.fail_at_eof = true;
        let mut reader = ReadFile::open(&mut transport, url()).await.unwrap();
        assert_eq!(reader.read(&mut [0; 1]).await.unwrap(), 0);
    });
}

#[test]
fn dropping_partial_reader_does_not_drain_or_close() {
    block_on(async {
        let mut transport = Transport::new(response("206 Partial Content", &[42; 8192]));
        let mut reader = ReadFile::open(&mut transport, url()).await.unwrap();
        assert_eq!(reader.read(&mut [0; 1]).await.unwrap(), 1);
        drop(reader);
        assert!(transport.read_at < transport.response.len());
        assert_eq!(transport.closes, 0);
    });
}

#[test]
fn malformed_headers_and_missing_length_are_rejected() {
    for bytes in [
        "HTTP/1.0 206 Partial Content\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 nope Invalid\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 206 Partial Content\r\nBroken\r\n\r\n",
        "HTTP/1.1 206 Partial Content\r\nContent-Length: nope\r\n\r\n",
        "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        "HTTP/1.1 206 Partial Content\r\nTransfer-Encoding: chunked\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\n\r\nab",
        "HTTP/1.1 206 Partial Content\r\nContent-Length: 1",
    ] {
        assert!(
            matches!(
                block_on(ReadFile::open(Transport::new(bytes.as_bytes()), url())),
                Err(Error::HttpParseError(_))
            ),
            "{bytes:?}"
        );
    }
    for bytes in [
        "HTTP/1.1 206 Partial Content\r\n\r\n",
        "HTTP/1.1 206 Partial Content\r\nTransfer-Encoding: chunked\r\n\r\n",
    ] {
        assert!(matches!(
            block_on(ReadFile::open(Transport::new(bytes.as_bytes()), url())),
            Err(Error::HttpParseError(HttpError::MissingContentLength))
        ));
    }
}

#[test]
fn server_errors_keep_status_and_bounded_body() {
    for status in [
        "403 Forbidden",
        "416 Range Not Satisfiable",
        "500 Internal Server Error",
    ] {
        let error = block_on(ReadFile::open(
            Transport::new(response(status, b"failed")),
            url(),
        ))
        .err()
        .unwrap();
        assert!(matches!(error, Error::HttpStatus { status: actual, body }
            if actual == status[..3].parse::<u16>().unwrap() && body == b"failed"));
    }
    let bytes = b"HTTP/1.1 429 Too Many Requests\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nslow\r\n0\r\n\r\n";
    assert!(
        matches!(block_on(ReadFile::open(Transport::new(bytes.as_slice()), url())),
        Err(Error::HttpStatus { status: 429, body }) if body == b"slow")
    );
    let bytes = b"HTTP/1.1 500 Error\r\nContent-Length: 16385\r\n\r\n";
    assert!(matches!(
        block_on(ReadFile::open(Transport::new(bytes.as_slice()), url())),
        Err(Error::HttpParseError(HttpError::BodyTooLarge {
            limit: 16384,
            size: 16385
        }))
    ));
}

#[test]
fn ignored_specific_range_is_rejected() {
    for (start, end) in [(1, None), (0, Some(4))] {
        assert!(matches!(block_on(ReadFile::open_with_range(
            Transport::new(response("200 OK", b"full file")), url(), start, end)),
            Err(Error::HttpStatus { status: 200, body }) if body == b"full file"));
    }
}

#[test]
fn truncated_body_returns_unexpected_eof() {
    block_on(async {
        let bytes = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\r\nab";
        let mut reader = ReadFile::open(Transport::new(bytes.as_slice()), url())
            .await
            .unwrap();
        let mut body = Vec::new();
        assert_eq!(
            reader.read_to_end(&mut body).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(body, b"ab");
    });
}

#[test]
fn opening_transport_failures_keep_their_error_kinds() {
    for failure in [0, 1, 2] {
        let mut transport = Transport::new(response("206 Partial Content", b"abc"));
        transport.fail_write = failure == 0;
        transport.fail_flush = failure == 1;
        transport.fail_read = failure == 2;
        let expected = if failure == 2 {
            io::ErrorKind::ConnectionReset
        } else {
            io::ErrorKind::BrokenPipe
        };
        assert!(matches!(block_on(ReadFile::open(transport, url())),
            Err(Error::IoError(error)) if error.kind() == expected));
    }
}

#[test]
fn body_transport_error_passes_through() {
    block_on(async {
        let bytes = response("206 Partial Content", b"abcdef");
        let mut transport = Transport::new(bytes.clone());
        transport.read_limit = 1;
        transport.fail_read_at = Some(bytes.len() - 6);
        let mut reader = ReadFile::open(transport, url()).await.unwrap();
        let error = reader.read(&mut [0; 1]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(error.to_string(), "read failed");
    });
}
