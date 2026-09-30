//! Renewal requests, attachment selection, and failure handling.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use discord_webhook_storage::{
    DiscordFile, DiscordFileUrl, Error, HttpError, ResponseError, WebhookCredentials,
};
use futures::{AsyncRead, AsyncWrite, executor::block_on, io::Cursor};
use serde_json::json;

const OLD_URL: &str = "https://cdn.discordapp.com/attachments/123/456/file.bin?ex=1&is=0&hm=aa";
const NEW_URL: &str =
    "https://cdn.discordapp.com/attachments/123/456/file.bin?ex=ffffffffffffffff&is=2&hm=bb";

struct Connection {
    response: Cursor<Vec<u8>>,
    written: Vec<u8>,
    flushed: bool,
    pending: bool,
    fail: Option<&'static str>,
}

impl Connection {
    fn new(response: Vec<u8>) -> Self {
        Self {
            response: Cursor::new(response),
            written: Vec::new(),
            flushed: false,
            pending: false,
            fail: None,
        }
    }

    fn yield_once(&mut self, cx: &Context<'_>) -> bool {
        self.pending = !self.pending;
        if self.pending {
            cx.waker().wake_by_ref();
        }
        self.pending
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.fail == Some("read") {
            return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
        }
        assert!(self.flushed);
        assert!(self.written.ends_with(b"\r\n\r\n"));
        if self.yield_once(cx) {
            return Poll::Pending;
        }
        let size = buf.len().min(1);
        Pin::new(&mut self.response).poll_read(cx, &mut buf[..size])
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.fail == Some("write") {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if self.fail == Some("zero") {
            return Poll::Ready(Ok(0));
        }
        if self.yield_once(cx) {
            return Poll::Pending;
        }
        let size = buf.len().min(1);
        self.written.extend_from_slice(&buf[..size]);
        Poll::Ready(Ok(size))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.fail == Some("flush") {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        self.flushed = true;
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        panic!("renewal must not close the caller's connection")
    }
}

fn file() -> DiscordFile {
    DiscordFile {
        id: "789".to_owned(),
        url: DiscordFileUrl::parse(OLD_URL).unwrap(),
    }
}

fn credentials() -> WebhookCredentials<'static> {
    WebhookCredentials::parse("https://discord.com/api/webhooks/111/token").unwrap()
}

fn response(status: u16, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\n\r\n",
        body.len(),
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn message() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "789",
        "attachments": [
            {"id": "999", "url": "https://example.com/another-file"},
            {"id": "456", "url": NEW_URL},
        ],
    }))
    .unwrap()
}

#[test]
fn file_validity_delegates_to_url() {
    let mut stored = file();
    assert!(!stored.is_valid());
    assert_eq!(stored.is_valid(), stored.url.is_valid());
    stored.url.ex = u64::MAX;
    assert!(stored.is_valid());
    assert_eq!(stored.is_valid(), stored.url.is_valid());
}

#[test]
fn renews_expired_url_with_partial_io_and_selects_matching_attachment() {
    let mut stored = file();
    let mut connection = Connection::new(response(200, &message()));
    block_on(stored.renew(&mut connection, &credentials())).unwrap();

    assert_eq!(stored.id, "789");
    assert_eq!(stored.url, DiscordFileUrl::parse(NEW_URL).unwrap());
    assert!(stored.is_valid());
    assert_eq!(
        connection.written,
        b"GET /api/webhooks/111/token/messages/789 HTTP/1.1\r\nHost: discord.com\r\nAccept: application/json\r\n\r\n",
    );
}

#[test]
fn renews_from_chunked_response() {
    let body = message();
    let split = body.len() / 2;
    let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for chunk in [&body[..split], &body[split..]] {
        bytes.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"0\r\n\r\n");
    let mut stored = file();
    block_on(stored.renew(Connection::new(bytes), &credentials())).unwrap();
    assert_eq!(stored.url.to_string(), NEW_URL);
}

#[test]
fn preserves_http_error_status_and_body_without_mutating_file() {
    for (status, body) in [
        (
            401,
            br#"{"message":"Invalid Webhook Token","code":50027}"#.as_slice(),
        ),
        (403, b"Forbidden".as_slice()),
        (
            404,
            br#"{"message":"Unknown Message","code":10008}"#.as_slice(),
        ),
        (
            429,
            br#"{"message":"Rate limited","retry_after":1}"#.as_slice(),
        ),
        (500, b"Internal Server Error".as_slice()),
        (302, b"Redirect".as_slice()),
    ] {
        let mut stored = file();
        let before = stored.clone();
        let error = block_on(stored.renew(Connection::new(response(status, body)), &credentials()))
            .unwrap_err();
        assert!(
            matches!(error, Error::HttpStatus { status: actual, body: actual_body }
            if actual == status && actual_body == body)
        );
        assert_eq!(stored, before);
    }
}

#[test]
fn rejects_bodyless_unsuccessful_status() {
    let mut stored = file();
    let before = stored.clone();
    let error = block_on(stored.renew(
        Connection::new(b"HTTP/1.1 204 No Content\r\n\r\n".to_vec()),
        &credentials(),
    ))
    .unwrap_err();
    assert!(matches!(error, Error::HttpStatus { status: 204, body } if body.is_empty()));
    assert_eq!(stored, before);
}

#[test]
fn rejects_invalid_json_and_attachment_fields_without_mutating_file() {
    for body in [
        b"not json".as_slice(),
        b"{}",
        br#"{"attachments":null}"#,
        br#"{"attachments":[null]}"#,
        br#"{"attachments":[{"id":"456"}]}"#,
        br#"{"attachments":[{"id":456,"url":"url"}]}"#,
        br#"{"attachments":[{"id":"456","url":null}]}"#,
    ] {
        let mut stored = file();
        let before = stored.clone();
        assert!(matches!(
            block_on(stored.renew(Connection::new(response(200, body)), &credentials())),
            Err(Error::JsonError(_)),
        ));
        assert_eq!(stored, before);
    }
}

#[test]
fn rejects_missing_attachment_and_invalid_url_without_mutating_file() {
    for attachments in [json!([]), json!([{"id": "999", "url": NEW_URL}])] {
        let body = serde_json::to_vec(&json!({"attachments": attachments})).unwrap();
        let mut stored = file();
        let before = stored.clone();
        assert!(matches!(
            block_on(stored.renew(Connection::new(response(200, &body)), &credentials())),
            Err(Error::InvalidResponse(ResponseError::AttachmentNotFound {
                attachment_id: 456,
            })),
        ));
        assert_eq!(stored, before);
    }
    let mut stored = file();
    let before = stored.clone();
    let body = br#"{"attachments":[{"id":"456","url":"https://example.com/file"}]}"#;
    assert!(matches!(
        block_on(stored.renew(Connection::new(response(200, body)), &credentials())),
        Err(Error::InvalidDiscordFileUrl(_)),
    ));
    assert_eq!(stored, before);
}

#[test]
fn rejects_malformed_oversized_and_truncated_http_without_mutating_file() {
    for bytes in [
        b"HTTP/1.1 invalid OK\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: invalid\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 16385\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4001\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{}".to_vec(),
    ] {
        let mut stored = file();
        let before = stored.clone();
        let result = block_on(stored.renew(Connection::new(bytes), &credentials()));
        assert!(matches!(
            result,
            Err(Error::HttpParseError(_) | Error::IoError(_))
        ));
        assert_eq!(stored, before);
    }
    let mut stored = file();
    assert!(matches!(
        block_on(stored.renew(
            Connection::new(response(200, &vec![b' '; 16385])),
            &credentials(),
        )),
        Err(Error::HttpParseError(HttpError::BodyTooLarge {
            limit: 16384,
            ..
        })),
    ));
}

#[test]
fn propagates_transport_errors_without_mutating_file() {
    for (operation, kind) in [
        ("write", io::ErrorKind::BrokenPipe),
        ("zero", io::ErrorKind::WriteZero),
        ("flush", io::ErrorKind::BrokenPipe),
        ("read", io::ErrorKind::ConnectionReset),
    ] {
        let mut connection = Connection::new(response(200, &message()));
        connection.fail = Some(operation);
        let mut stored = file();
        let before = stored.clone();
        assert!(matches!(
            block_on(stored.renew(&mut connection, &credentials())),
            Err(Error::IoError(error)) if error.kind() == kind,
        ));
        assert_eq!(stored, before);
    }
}

#[test]
fn encodes_request_path_segments() {
    let credentials =
        WebhookCredentials::parse("https://discord.com/api/webhooks/111/token?x\r\n").unwrap();
    let mut stored = file();
    stored.id = "789/other?x".to_owned();
    let mut connection = Connection::new(response(404, b"Unknown Message"));
    assert!(block_on(stored.renew(&mut connection, &credentials)).is_err());
    assert!(connection.written.starts_with(
        b"GET /api/webhooks/111/token%3Fx%0D%0A/messages/789%2Fother%3Fx HTTP/1.1\r\n",
    ));
}
