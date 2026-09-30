//! Renewal requests, attachment selection, and failure handling.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use discord_webhook_storage::{
    BotCredentials, DiscordFile, DiscordFileUrl, Error, HttpError, ResponseError,
    WebhookCredentials, renew_urls,
};
use futures::{AsyncRead, AsyncWrite, executor::block_on, io::Cursor};
use serde_json::json;

const OLD_URL: &str = "https://cdn.discordapp.com/attachments/123/456/file.bin?ex=1&is=0&hm=aa";
const NEW_URL: &str =
    "https://cdn.discordapp.com/attachments/123/456/file.bin?ex=ffffffffffffffff&is=2&hm=bb";

struct Connection {
    response: Cursor<Vec<u8>>,
    written: Vec<u8>,
    requests: Vec<Vec<u8>>,
    request_at: usize,
    flushed: bool,
    pending: bool,
    fail: Option<&'static str>,
}

impl Connection {
    fn new(response: Vec<u8>) -> Self {
        Self {
            response: Cursor::new(response),
            written: Vec::new(),
            requests: Vec::new(),
            request_at: 0,
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
        self.flushed = false;
        self.written.extend_from_slice(&buf[..size]);
        Poll::Ready(Ok(size))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.fail == Some("flush") {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        let request = self.written[self.request_at..].to_vec();
        let separator = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        let headers = std::str::from_utf8(&request[..separator]).unwrap();
        let size = headers
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(0);
        assert_eq!(request.len() - separator - 4, size);
        self.requests.push(request);
        self.request_at = self.written.len();
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

fn bot_credentials() -> BotCredentials<'static> {
    BotCredentials {
        token: "secret-bot-token",
    }
}

fn refreshed_url(url: &DiscordFileUrl) -> DiscordFileUrl {
    let mut refreshed = url.clone();
    refreshed.ex = u64::MAX;
    refreshed.hm = "bb".to_owned();
    refreshed
}

fn refresh_response(urls: &[DiscordFileUrl]) -> Vec<u8> {
    let entries: Vec<_> = urls
        .iter()
        .rev()
        .map(|url| {
            json!({
                "original": url.to_string(),
                "refreshed": refreshed_url(url).to_string(),
            })
        })
        .collect();
    response(
        200,
        &serde_json::to_vec(&json!({"refreshed_urls": entries})).unwrap(),
    )
}

fn request_json(request: &[u8]) -> serde_json::Value {
    let separator = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap();
    serde_json::from_slice(&request[separator + 4..]).unwrap()
}

#[test]
fn renews_urls_in_place_in_input_order_including_valid_and_repeated_urls() {
    let mut urls = vec![
        file().url,
        DiscordFileUrl::parse(NEW_URL).unwrap(),
        file().url,
    ];
    let originals = urls.clone();
    let expected: Vec<_> = originals.iter().map(refreshed_url).collect();
    let mut connection = Connection::new(refresh_response(&urls));
    block_on(renew_urls(&mut connection, &bot_credentials(), &mut urls)).unwrap();
    assert_eq!(urls, expected);
    assert_eq!(connection.requests.len(), 1);
    let request = &connection.requests[0];
    let headers = std::str::from_utf8(request).unwrap();
    assert!(headers.starts_with("POST /api/v9/attachments/refresh-urls HTTP/1.1\r\n"));
    assert!(headers.contains("\r\nHost: discord.com\r\n"));
    assert!(headers.contains("\r\nAuthorization: Bot secret-bot-token\r\n"));
    assert!(headers.contains("\r\nContent-Type: application/json\r\n"));
    assert_eq!(
        request_json(request),
        json!({
            "attachment_urls": originals.iter().map(ToString::to_string).collect::<Vec<_>>(),
        })
    );
}

#[test]
fn renews_arbitrary_number_of_urls_in_batches_and_accepts_large_response() {
    let mut urls: Vec<_> = (0..121)
        .map(|index| {
            let mut url = file().url;
            url.attachment_id = 1000 + index;
            url.attachment_name = "x".repeat(250);
            url
        })
        .collect();
    let originals = urls.clone();
    let mut connection = Connection::new(urls.chunks(50).flat_map(refresh_response).collect());
    block_on(renew_urls(
        &mut connection,
        &bot_credentials(),
        urls.iter_mut(),
    ))
    .unwrap();
    assert_eq!(
        urls,
        originals.iter().map(refreshed_url).collect::<Vec<_>>()
    );
    assert_eq!(connection.requests.len(), 3);
    for (request, batch) in connection.requests.iter().zip(originals.chunks(50)) {
        assert_eq!(
            request_json(request),
            json!({
                "attachment_urls": batch.iter().map(ToString::to_string).collect::<Vec<_>>(),
            })
        );
    }
}

#[test]
fn accepts_iterator_of_file_url_references_and_empty_input() {
    let mut files = [file(), file()];
    let urls: Vec<_> = files.iter().map(|file| file.url.clone()).collect();
    block_on(renew_urls(
        Connection::new(refresh_response(&urls)),
        &bot_credentials(),
        files.iter_mut().map(|file| &mut file.url),
    ))
    .unwrap();
    assert!(files.iter().all(DiscordFile::is_valid));
    assert!(files.iter().all(|file| file.id == "789"));

    let mut connection = Connection::new(Vec::new());
    let mut empty: Vec<DiscordFileUrl> = Vec::new();
    block_on(renew_urls(&mut connection, &bot_credentials(), &mut empty)).unwrap();
    assert!(connection.written.is_empty());
}

#[test]
fn invalid_bot_tokens_are_rejected_before_io_and_debug_hides_token() {
    assert!(!format!("{:?}", bot_credentials()).contains("secret-bot-token"));
    for token in [
        "",
        "Bot token",
        "token\r\nInjected: header",
        "token\0",
        "tökén",
    ] {
        let mut connection = Connection::new(Vec::new());
        let mut urls = [file().url];
        assert!(matches!(
            block_on(renew_urls(
                &mut connection,
                &BotCredentials { token },
                &mut urls
            )),
            Err(Error::InvalidBotToken),
        ));
        assert!(connection.written.is_empty());
    }
}

#[test]
fn batch_http_errors_preserve_status_body_and_urls() {
    for status in [401, 403, 404, 429, 500] {
        let body = br#"{"message":"Rejected","retry_after":1}"#;
        let mut urls = [file().url];
        let before = urls.clone();
        assert!(matches!(
            block_on(renew_urls(
                Connection::new(response(status, body)), &bot_credentials(), &mut urls,
            )),
            Err(Error::HttpStatus { status: actual, body: actual_body })
                if actual == status && actual_body == body,
        ));
        assert_eq!(urls, before);
    }
}

#[test]
fn batch_json_and_invalid_url_errors_preserve_urls() {
    for body in [
        b"not json".as_slice(),
        b"{}",
        br#"{"refreshed_urls":null}"#,
        br#"{"refreshed_urls":[{"original":1,"refreshed":"url"}]}"#,
        br#"{"refreshed_urls":[{"original":"url"}]}"#,
    ] {
        let mut urls = [file().url];
        let before = urls.clone();
        assert!(matches!(
            block_on(renew_urls(
                Connection::new(response(200, body)),
                &bot_credentials(),
                &mut urls,
            )),
            Err(Error::JsonError(_)),
        ));
        assert_eq!(urls, before);
    }
    let mut urls = [file().url];
    let before = urls.clone();
    let body = serde_json::to_vec(&json!({"refreshed_urls": [{
        "original": urls[0].to_string(), "refreshed": "https://example.com/file",
    }]}))
    .unwrap();
    assert!(matches!(
        block_on(renew_urls(
            Connection::new(response(200, &body)),
            &bot_credentials(),
            &mut urls,
        )),
        Err(Error::InvalidDiscordFileUrl(_)),
    ));
    assert_eq!(urls, before);
}

#[test]
fn missing_unexpected_and_conflicting_batch_results_preserve_whole_batch() {
    let mut second = file().url;
    second.attachment_id = 789;
    let urls = [file().url, second];
    let valid_entry = json!({
        "original": urls[0].to_string(), "refreshed": NEW_URL,
    });
    for entries in [
        json!([valid_entry.clone()]),
        json!([{"original": "unexpected", "refreshed": NEW_URL}]),
        json!([
            valid_entry,
            {"original": urls[0].to_string(), "refreshed": OLD_URL},
        ]),
    ] {
        let mut targets = urls.clone();
        let body = serde_json::to_vec(&json!({"refreshed_urls": entries})).unwrap();
        assert!(matches!(
            block_on(renew_urls(
                Connection::new(response(200, &body)),
                &bot_credentials(),
                &mut targets,
            )),
            Err(Error::InvalidResponse(
                ResponseError::MissingRefreshedUrl { index: 1 }
                    | ResponseError::InvalidRefreshedUrls,
            )),
        ));
        assert_eq!(targets, urls);
    }
}

#[test]
fn later_failed_batch_keeps_prior_updates_and_reports_global_missing_index() {
    let mut urls: Vec<_> = (0..51)
        .map(|index| {
            let mut url = file().url;
            url.attachment_id += index;
            url
        })
        .collect();
    let before = urls.clone();
    let mut bytes = refresh_response(&urls[..50]);
    bytes.extend_from_slice(&response(200, br#"{"refreshed_urls":[]}"#));
    assert!(matches!(
        block_on(renew_urls(
            Connection::new(bytes),
            &bot_credentials(),
            &mut urls
        )),
        Err(Error::InvalidResponse(ResponseError::MissingRefreshedUrl {
            index: 50
        })),
    ));
    assert_eq!(
        urls[..50],
        before[..50].iter().map(refreshed_url).collect::<Vec<_>>()
    );
    assert_eq!(urls[50], before[50]);
}

#[test]
fn batch_transport_and_http_parse_failures_preserve_urls() {
    for operation in ["write", "zero", "flush", "read"] {
        let mut connection = Connection::new(refresh_response(&[file().url]));
        connection.fail = Some(operation);
        let mut urls = [file().url];
        let before = urls.clone();
        assert!(matches!(
            block_on(renew_urls(connection, &bot_credentials(), &mut urls)),
            Err(Error::IoError(_)),
        ));
        assert_eq!(urls, before);
    }
    for bytes in [
        b"HTTP/1.1 invalid OK\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{}".to_vec(),
    ] {
        let mut urls = [file().url];
        let before = urls.clone();
        assert!(matches!(
            block_on(renew_urls(
                Connection::new(bytes),
                &bot_credentials(),
                &mut urls
            )),
            Err(Error::HttpParseError(_) | Error::IoError(_)),
        ));
        assert_eq!(urls, before);
    }
}

#[test]
fn renews_batch_from_chunked_response() {
    let mut urls = [file().url];
    let entries = json!({"refreshed_urls": [{"original": OLD_URL, "refreshed": NEW_URL}]});
    let body = serde_json::to_vec(&entries).unwrap();
    let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    bytes.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(b"\r\n0\r\n\r\n");
    block_on(renew_urls(
        Connection::new(bytes),
        &bot_credentials(),
        &mut urls,
    ))
    .unwrap();
    assert_eq!(urls[0].to_string(), NEW_URL);
}
