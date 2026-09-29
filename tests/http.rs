// The parser is private to the crate. Include it here to test its incremental
// behavior without changing the public API.
pub use discord_webhook_storage::{Error, Result};
#[path = "../src/http.rs"]
mod http;

use http::HttpStatusParser;
use futures::{AsyncRead, executor::block_on, io::Cursor};
use std::{io, pin::Pin, task::{Context, Poll}};

struct Fragmented<'a>(&'a [u8]);

impl AsyncRead for Fragmented<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let n = buf.len().min(self.0.len()).min(1);
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Poll::Ready(Ok(n))
    }
}

#[test]
fn status_line_can_arrive_in_fragments() {
    let mut parser = HttpStatusParser::new(Fragmented(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(block_on(parser.status()).unwrap(), 200);
}

#[test]
fn status_rejects_bad_version_code_and_utf8() {
    for line in [
        b"HTTP/1.0 200 OK\r\n".as_slice(),
        b"HTTP/1.1 nope OK\r\n",
        b"HTTP/1.1 200\r\n",
        b"HTTP/1.1 200\xff\r\n",
    ] {
        let mut parser = HttpStatusParser::new(Cursor::new(line));
        assert!(block_on(parser.status()).is_err());
    }
}

#[test]
fn headers_are_case_insensitive_and_keep_body_bytes() {
    let mut status = HttpStatusParser::new(Cursor::new(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nX-Test:  a  \r\nCONTENT-LENGTH: 4\r\n\r\nbody"));
    assert_eq!(block_on(status.status()).unwrap(), 200);
    let mut headers = status.into_headers();
    assert_eq!(block_on(headers.next_header()).unwrap(), Some(("content-length", "4")));
    assert_eq!(block_on(headers.next_header()).unwrap(), Some(("X-Test", "a")));
    assert_eq!(block_on(headers.next_header()).unwrap(), Some(("CONTENT-LENGTH", "4")));
    assert_eq!(block_on(headers.next_header()).unwrap(), None);
    assert!(headers.is_complete());
    assert_eq!(headers.body_size(), Some(4));
    assert_eq!(block_on(headers.next_header()).unwrap(), None);
    assert_eq!(headers.remaining().1, b"body");
}

#[test]
fn headers_can_arrive_in_fragments() {
    let mut status = HttpStatusParser::new(Fragmented(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"));
    block_on(status.status()).unwrap();
    let mut headers = status.into_headers();
    assert!(!headers.is_complete());
    assert_eq!(block_on(headers.next_header()).unwrap(), Some(("Content-Length", "3")));
    assert!(!headers.is_complete());
    assert_eq!(block_on(headers.next_header()).unwrap(), None);
    assert!(headers.is_complete());
    assert_eq!(block_on(headers.body(3)).unwrap(), b"abc");
}

#[test]
fn headers_reject_conflicting_or_invalid_lengths() {
    for lines in [
        b"Content-Length: 3\r\nContent-Length: 4\r\n".as_slice(),
        b"Content-Length: nope\r\n",
        b"Content-Length: -1\r\n",
        b"Content-Length: 999999999999999999999999999999999\r\n",
    ] {
        let bytes = [b"HTTP/1.1 200 OK\r\n".as_slice(), lines].concat();
        let mut status = HttpStatusParser::new(Cursor::new(bytes));
        block_on(status.status()).unwrap();
        let mut headers = status.into_headers();
        loop {
            match block_on(headers.next_header()) {
                Ok(Some(_)) => continue,
                Err(Error::HttpParseError) => break,
                other => panic!("expected invalid content length, got {other:?}"),
            }
        }
    }
}

#[test]
fn headers_reject_missing_colon_and_invalid_utf8() {
    for line in [b"Broken\r\n".as_slice(), b"X-Test: \xff\r\n"] {
        let bytes = [b"HTTP/1.1 200 OK\r\n".as_slice(), line].concat();
        let mut status = HttpStatusParser::new(Cursor::new(bytes));
        block_on(status.status()).unwrap();
        let mut headers = status.into_headers();
        assert_eq!(block_on(headers.next_header()), Err(Error::HttpParseError));
    }
}

#[test]
fn missing_content_length_is_visible() {
    let mut status = HttpStatusParser::new(Cursor::new(b"HTTP/1.1 200 OK\r\nX-Test: yes\r\n\r\n"));
    block_on(status.status()).unwrap();
    let mut headers = status.into_headers();
    assert_eq!(block_on(headers.next_header()).unwrap(), Some(("X-Test", "yes")));
    assert_eq!(block_on(headers.next_header()).unwrap(), None);
    assert!(headers.is_complete());
    assert_eq!(headers.body_size(), None);
}

#[test]
fn many_headers_compact_consumed_input() {
    let bytes = format!("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n{}\r\nbody", "X-Header: value\r\n".repeat(500));
    let mut status = HttpStatusParser::new(Cursor::new(bytes.into_bytes()));
    block_on(status.status()).unwrap();
    let mut headers = status.into_headers();
    assert_eq!(block_on(headers.next_header()).unwrap(), Some(("Content-Length", "4")));
    for _ in 0..500 {
        assert_eq!(block_on(headers.next_header()).unwrap(), Some(("X-Header", "value")));
    }
    assert_eq!(block_on(headers.next_header()).unwrap(), None);
    assert_eq!(block_on(headers.body(4)).unwrap(), b"body");
}
