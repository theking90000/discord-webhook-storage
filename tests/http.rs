// The parser is private to the crate. Include it here to test its incremental
// behavior without changing the public API.
pub use discord_webhook_storage::{Error, Result};
#[path = "../src/http.rs"]
mod http;

use http::HttpStatusParser;

#[test]
fn status_line_can_arrive_in_fragments() {
    let mut parser = HttpStatusParser::new();
    parser.feed(b"HTTP/1.1 20");
    assert_eq!(parser.status().unwrap(), None);
    parser.feed(b"0 OK\r");
    assert_eq!(parser.status().unwrap(), None);
    parser.feed(b"\n");
    assert_eq!(parser.status().unwrap(), Some(200));
}

#[test]
fn status_rejects_bad_version_code_and_utf8() {
    for line in [
        b"HTTP/1.0 200 OK\r\n".as_slice(),
        b"HTTP/1.1 nope OK\r\n",
        b"HTTP/1.1 200\r\n",
        b"HTTP/1.1 200\xff\r\n",
    ] {
        let mut parser = HttpStatusParser::new();
        parser.feed(line);
        assert!(parser.status().is_err());
    }
}

#[test]
fn headers_are_case_insensitive_and_keep_body_bytes() {
    let mut status = HttpStatusParser::new();
    status.feed(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nX-Test:  a  \r\nCONTENT-LENGTH: 4\r\n\r\nbody");
    assert_eq!(status.status().unwrap(), Some(200));
    let mut headers = status.into_headers();
    assert_eq!(headers.next_header().unwrap(), Some(("content-length", "4")));
    assert_eq!(headers.next_header().unwrap(), Some(("X-Test", "a")));
    assert_eq!(headers.next_header().unwrap(), Some(("CONTENT-LENGTH", "4")));
    assert_eq!(headers.next_header().unwrap(), None);
    assert!(headers.is_complete());
    assert_eq!(headers.body_size(), Some(4));
    assert_eq!(headers.next_header().unwrap(), None);
    assert_eq!(headers.remaining(), b"body");
}

#[test]
fn headers_can_arrive_in_fragments() {
    let mut status = HttpStatusParser::new();
    status.feed(b"HTTP/1.1 200 OK\r\nContent-Len");
    status.status().unwrap();
    let mut headers = status.into_headers();
    assert_eq!(headers.next_header().unwrap(), None);
    assert!(!headers.is_complete());
    headers.feed(b"gth: 3\r\n\r");
    assert_eq!(headers.next_header().unwrap(), Some(("Content-Length", "3")));
    assert_eq!(headers.next_header().unwrap(), None);
    headers.feed(b"\nabc");
    assert_eq!(headers.next_header().unwrap(), None);
    assert!(headers.is_complete());
    assert_eq!(headers.remaining(), b"abc");
}

#[test]
fn headers_reject_conflicting_or_invalid_lengths() {
    for lines in [
        b"Content-Length: 3\r\nContent-Length: 4\r\n".as_slice(),
        b"Content-Length: nope\r\n",
        b"Content-Length: -1\r\n",
        b"Content-Length: 999999999999999999999999999999999\r\n",
    ] {
        let mut status = HttpStatusParser::new();
        status.feed(b"HTTP/1.1 200 OK\r\n");
        status.status().unwrap();
        let mut headers = status.into_headers();
        headers.feed(lines);
        loop {
            match headers.next_header() {
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
        let mut status = HttpStatusParser::new();
        status.feed(b"HTTP/1.1 200 OK\r\n");
        status.status().unwrap();
        let mut headers = status.into_headers();
        headers.feed(line);
        assert_eq!(headers.next_header(), Err(Error::HttpParseError));
    }
}

#[test]
fn missing_content_length_is_visible() {
    let mut status = HttpStatusParser::new();
    status.feed(b"HTTP/1.1 200 OK\r\nX-Test: yes\r\n\r\n");
    status.status().unwrap();
    let mut headers = status.into_headers();
    assert_eq!(headers.next_header().unwrap(), Some(("X-Test", "yes")));
    assert_eq!(headers.next_header().unwrap(), None);
    assert!(headers.is_complete());
    assert_eq!(headers.body_size(), None);
}

#[test]
fn many_headers_compact_consumed_input() {
    let mut status = HttpStatusParser::new();
    status.feed(b"HTTP/1.1 200 OK\r\n");
    status.status().unwrap();
    let mut headers = status.into_headers();
    for _ in 0..500 {
        headers.feed(b"X-Header: value\r\n");
        assert_eq!(headers.next_header().unwrap(), Some(("X-Header", "value")));
    }
    headers.feed(b"\r\nbody");
    assert_eq!(headers.next_header().unwrap(), None);
    assert_eq!(headers.remaining(), b"body");
}
