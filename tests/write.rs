//! Check upload framing, finalization, payload limits, and response handling.

use std::{
    cell::RefCell,
    io::{self, IoSlice},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use discord_webhook_storage::{Error, WebhookCredentials, WriteConfig, WriteFile};
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt, executor::block_on};
use serde_json::json;

#[derive(Default)]
struct State {
    response: Vec<u8>,
    read_at: usize,
    written: Vec<u8>,
    max_read: usize,
    max_write: usize,
    flushes: usize,
    closes: usize,
    require_complete_request: bool,
    pending_writes: bool,
    yield_write: bool,
    pending_reads: bool,
    yield_read: bool,
    error_on_eof: bool,
    fail_read: bool,
    fail_write: bool,
    fail_flush: bool,
}

#[derive(Clone)]
struct Transport(Rc<RefCell<State>>);

impl Transport {
    fn new(response: Vec<u8>) -> Self {
        Self(Rc::new(RefCell::new(State {
            response,
            max_read: usize::MAX,
            max_write: usize::MAX,
            ..State::default()
        })))
    }

    fn written(&self) -> Vec<u8> {
        self.0.borrow().written.clone()
    }
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.borrow_mut();
        if state.fail_read {
            return Poll::Ready(Err(io::Error::other("read failed")));
        }
        if state.error_on_eof && state.read_at == state.response.len() {
            return Poll::Ready(Err(io::Error::other("read beyond response")));
        }
        if state.pending_reads {
            state.yield_read = !state.yield_read;
            if state.yield_read {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        if state.require_complete_request
            && (!state.written.ends_with(b"0\r\n\r\n") || state.flushes == 0)
        {
            return Poll::Ready(Err(io::Error::other("request body is incomplete")));
        }
        let n = buf
            .len()
            .min(state.max_read)
            .min(state.response.len() - state.read_at);
        buf[..n].copy_from_slice(&state.response[state.read_at..state.read_at + n]);
        state.read_at += n;
        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.borrow_mut();
        if state.fail_write {
            return Poll::Ready(Err(io::Error::other("write failed")));
        }
        if state.pending_writes {
            state.yield_write = !state.yield_write;
            if state.yield_write {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
        let n = buf.len().min(state.max_write);
        state.written.extend_from_slice(&buf[..n]);
        Poll::Ready(Ok(n))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.borrow_mut();
        if state.fail_write {
            return Poll::Ready(Err(io::Error::other("write failed")));
        }
        let mut left = state.max_write;
        let mut written = 0;
        for buf in bufs {
            let n = buf.len().min(left);
            state.written.extend_from_slice(&buf[..n]);
            left -= n;
            written += n;
            if left == 0 {
                break;
            }
        }
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.borrow_mut();
        state.flushes += 1;
        if state.fail_flush {
            Poll::Ready(Err(io::Error::other("flush failed")))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.borrow_mut().closes += 1;
        self.poll_flush(cx)
    }
}

fn credentials() -> WebhookCredentials<'static> {
    WebhookCredentials::parse("https://discord.com/api/webhooks/123/token").unwrap()
}

fn response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn opened(transport: &Transport) -> WriteFile<Transport> {
    block_on(WriteFile::open(
        transport.clone(),
        &credentials(),
        &WriteConfig::default(),
    ))
    .unwrap()
}

fn decode_request(request: &[u8]) -> (&str, Vec<u8>) {
    let header_end = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
    assert!(headers.contains("Transfer-Encoding: chunked\r\n"));
    let mut encoded = &request[header_end..];
    let mut body = Vec::new();
    loop {
        let line_end = encoded.windows(2).position(|w| w == b"\r\n").unwrap();
        let size =
            usize::from_str_radix(std::str::from_utf8(&encoded[..line_end]).unwrap(), 16).unwrap();
        encoded = &encoded[line_end + 2..];
        if size == 0 {
            assert_eq!(
                encoded, b"\r\n",
                "exactly one final chunk must end the request"
            );
            break;
        }
        body.extend_from_slice(&encoded[..size]);
        assert_eq!(&encoded[size..size + 2], b"\r\n");
        encoded = &encoded[size + 2..];
    }
    (headers, body)
}

#[test]
fn credentials_accept_url_and_try_from() {
    let url = "https://discord.com/api/webhooks/123/token";
    assert_eq!(
        WebhookCredentials::parse(url),
        WebhookCredentials::try_from(url)
    );
    for invalid in [
        "",
        "http://discord.com/api/webhooks/123/token",
        "https://discord.com/api/webhooks/123",
    ] {
        assert_eq!(
            WebhookCredentials::parse(invalid),
            Err(Error::InvalidWebhookUrl)
        );
    }
}

#[test]
fn open_writes_multipart_request_and_file_bytes() {
    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    block_on(async {
        file.write_all(&[0, 2, 3, 255]).await.unwrap();
        file.close().await.unwrap();
    });

    let request = transport.written();
    let (headers, body) = decode_request(&request);
    let request_text = String::from_utf8_lossy(&body);
    assert!(
        headers.starts_with(
            "POST /api/webhooks/123/token?wait=true HTTP/1.1\r\nHost: discord.com\r\n"
        )
    );
    let boundary = headers
        .lines()
        .find_map(|line| line.strip_prefix("Content-Type: multipart/form-data; boundary="))
        .unwrap()
        .trim_end_matches('\r');
    assert_eq!(boundary.len(), 56);
    assert!(
        request_text.contains("name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n")
    );
    assert!(request_text.contains("\"filename\":\"file.bin\""));
    assert!(request_text.contains("\"payload_limit\":20000000"));
    assert!(request_text.contains("name=\"files[0]\"; filename=\"file.bin\""));
    assert!(
        body.ends_with(
            &[0, 2, 3, 255]
                .into_iter()
                .chain(format!("\r\n--{boundary}--\r\n").bytes())
                .collect::<Vec<_>>()
        )
    );
    assert_eq!(transport.0.borrow().flushes, 1);
}

#[test]
fn zero_bytes_and_repeated_close_write_one_terminator() {
    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    block_on(async {
        file.close().await.unwrap();
        file.close().await.unwrap();
    });
    let request = transport.written();
    assert!(decode_request(&request).1.ends_with(b"--\r\n"));
    assert_eq!(transport.0.borrow().closes, 0);
    assert_eq!(transport.0.borrow().flushes, 1);
    assert_eq!(
        block_on(file.write(&[1])).unwrap_err().kind(),
        io::ErrorKind::Other
    );
}

#[test]
fn close_handles_partial_transport_writes() {
    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    transport.0.borrow_mut().max_write = 3;
    block_on(file.close()).unwrap();
    assert!(decode_request(&transport.written()).1.ends_with(b"--\r\n"));
    assert_eq!(transport.0.borrow().flushes, 1);
}

#[test]
fn open_retries_partial_transport_writes() {
    let transport = Transport::new(Vec::new());
    transport.0.borrow_mut().max_write = 3;
    let mut file = opened(&transport);
    block_on(file.close()).unwrap();
    let request = transport.written();
    let (headers, body) = decode_request(&request);
    assert!(headers.starts_with("POST /api/webhooks/123/token?wait=true HTTP/1.1\r\n"));
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("name=\"payload_json\""));
    assert!(body.contains("name=\"files[0]\""));
}

#[test]
fn file_limit_applies_to_scalar_and_vectored_writes() {
    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    let large = vec![7; 20_000_000];
    block_on(file.write_all(&large)).unwrap();
    assert_eq!(
        block_on(file.write(&[1])).unwrap_err().kind(),
        io::ErrorKind::Other
    );
    assert_eq!(
        block_on(file.write_vectored(&[IoSlice::new(&[1])]))
            .unwrap_err()
            .kind(),
        io::ErrorKind::Other
    );

    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    assert_eq!(
        block_on(file.write_vectored(&[IoSlice::new(&[1, 2]), IoSlice::new(&[3])])).unwrap(),
        2
    );
    block_on(file.write_all(&[4])).unwrap();
}

#[test]
fn flush_and_transport_errors_are_reported() {
    let transport = Transport::new(Vec::new());
    transport.0.borrow_mut().fail_write = true;
    assert!(matches!(
        block_on(WriteFile::open(
            transport,
            &credentials(),
            &WriteConfig::default()
        )),
        Err(Error::IoError)
    ));

    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    transport.0.borrow_mut().fail_write = true;
    assert!(block_on(file.write(&[1])).is_err());
    assert!(block_on(file.close()).is_err());

    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    transport.0.borrow_mut().fail_flush = true;
    assert!(block_on(file.flush()).is_err());
    assert!(block_on(file.close()).is_err());
}

#[test]
fn finish_reads_fragmented_success_response() {
    let body = r#"{"id":"message-1","attachments":[{"url":"https://cdn.example/file"}]}"#;
    for read_size in [1, 7, 1024] {
        let transport = Transport::new(response("200 OK", body));
        transport.0.borrow_mut().max_read = read_size;
        let file = opened(&transport);
        let result = block_on(file.finish()).unwrap();
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({"id":"message-1","url":"https://cdn.example/file"})
        );
    }
}

#[test]
fn finish_completes_request_before_reading_response() {
    let body = r#"{"id":"message-1","attachments":[{"url":"https://cdn.example/file"}]}"#;
    for write_size in [1, 3, usize::MAX] {
        let transport = Transport::new(response("200 OK", body));
        {
            let mut state = transport.0.borrow_mut();
            state.max_write = write_size;
            state.pending_writes = true;
            state.require_complete_request = true;
        }
        let mut file = opened(&transport);
        block_on(file.write_all(&[0, 2, 3, 255])).unwrap();
        block_on(file.finish()).unwrap();
        decode_request(&transport.written());
        assert_eq!(transport.0.borrow().closes, 0);
    }
}

#[test]
fn finish_rejects_http_status_and_header_errors() {
    let cases = [
        response("400 Bad Request", "{}"),
        b"not http\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nBad-Header\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 16385\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n".to_vec(),
        b"HTTP/1.1 200".to_vec(),
    ];
    for bytes in cases {
        let transport = Transport::new(bytes);
        assert!(matches!(
            block_on(opened(&transport).finish()),
            Err(Error::HttpParseError)
        ));
    }
}

#[test]
fn finish_rejects_invalid_json_and_missing_fields() {
    for body in [
        "not json",
        "{}",
        r#"{"id":"x"}"#,
        r#"{"id":5,"attachments":[{"url":"x"}]}"#,
        r#"{"id":"x","attachments":[]}"#,
        r#"{"id":"x","attachments":[{"url":7}]}"#,
    ] {
        let transport = Transport::new(response("200 OK", body));
        let expected = if body == "not json" {
            Error::JsonError
        } else {
            Error::HttpParseError
        };
        assert!(matches!(block_on(opened(&transport).finish()), Err(error) if error == expected));
    }
}

#[test]
fn finish_reports_read_failures_and_truncated_body() {
    let transport = Transport::new(Vec::new());
    transport.0.borrow_mut().fail_read = true;
    assert!(matches!(
        block_on(opened(&transport).finish()),
        Err(Error::IoError)
    ));

    let transport = Transport::new(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}".to_vec());
    assert!(matches!(
        block_on(opened(&transport).finish()),
        Err(Error::IoError)
    ));
}

fn chunked_response(encoded: &[u8]) -> Vec<u8> {
    let mut response = b"HTTP/1.1 200 OK\r\ntransfer-encoding: ChUnKeD\r\n\r\n".to_vec();
    response.extend_from_slice(encoded);
    response
}

#[test]
fn finish_decodes_fragmented_chunks_extensions_and_trailers_without_eof() {
    let json_body = br#"{"id":"message-1","attachments":[{"url":"https://cdn.example/file"}]}"#;
    for with_trailers in [false, true] {
        let mut encoded = Vec::new();
        for chunk in json_body.chunks(11) {
            encoded
                .extend_from_slice(format!("{:x};ignored=\"value\"\r\n", chunk.len()).as_bytes());
            encoded.extend_from_slice(chunk);
            encoded.extend_from_slice(b"\r\n");
        }
        encoded.extend_from_slice(b"0;ignored=value\r\n");
        if with_trailers {
            encoded.extend_from_slice(b"X-Checksum: ignored\r\nX-Other: value\r\n");
        }
        encoded.extend_from_slice(b"\r\n");
        for read_size in [1, 2, 7, 1024] {
            let transport = Transport::new(chunked_response(&encoded));
            {
                let mut state = transport.0.borrow_mut();
                state.max_read = read_size;
                state.pending_reads = true;
                state.error_on_eof = true;
            }
            let result = block_on(opened(&transport).finish()).unwrap();
            assert_eq!(
                serde_json::to_value(result).unwrap(),
                json!({"id":"message-1","url":"https://cdn.example/file"})
            );
            let state = transport.0.borrow();
            assert_eq!(state.read_at, state.response.len());
        }
    }
}

#[test]
fn finish_limits_decoded_chunked_body_size() {
    let json_body = r#"{"id":"x","attachments":[{"url":"x"}]}"#;
    let body = format!("{json_body}{}", " ".repeat(16384 - json_body.len()));
    let encoded = format!("4000\r\n{body}\r\n0\r\n\r\n");
    let transport = Transport::new(chunked_response(encoded.as_bytes()));
    block_on(opened(&transport).finish()).unwrap();

    // Reject an oversized announcement before waiting for its data, and also
    // reject multiple chunks whose combined decoded size exceeds the limit.
    for encoded in ["4001\r\n".to_string(), format!("4000\r\n{body}\r\n1\r\n")] {
        let transport = Transport::new(chunked_response(encoded.as_bytes()));
        assert!(matches!(
            block_on(opened(&transport).finish()),
            Err(Error::HttpParseError)
        ));
    }
}

#[test]
fn finish_rejects_invalid_chunk_framing_and_trailers() {
    for encoded in [
        b"Z\r\n".as_slice(),
        b"+1\r\nx\r\n0\r\n\r\n",
        b"\r\n\r\n",
        b";extension\r\n\r\n",
        b"10000000000000000\r\n",
        b"FFFFFFFFFFFFFFFF\r\n",
        b"1\nx\r\n0\r\n\r\n",
        b"1\r\nxXX0\r\n\r\n",
        b"0\r\nBroken-Trailer\r\n\r\n",
    ] {
        let transport = Transport::new(chunked_response(encoded));
        assert!(
            matches!(
                block_on(opened(&transport).finish()),
                Err(Error::HttpParseError)
            ),
            "{encoded:?}"
        );
    }
    let oversized_line = format!("1;{}\r\n", "x".repeat(8192));
    let oversized_trailers = format!("0\r\n{}\r\n", "X-Trailer: x\r\n".repeat(1400));
    for encoded in [oversized_line, oversized_trailers] {
        let transport = Transport::new(chunked_response(encoded.as_bytes()));
        assert!(matches!(
            block_on(opened(&transport).finish()),
            Err(Error::HttpParseError)
        ));
    }
}

#[test]
fn finish_rejects_truncated_chunked_responses() {
    for encoded in [
        b"".as_slice(),
        b"1\r",
        b"3\r\nx",
        b"1\r\nx",
        b"1\r\nx\r\n",
        b"0\r\n",
        b"0\r\nX-Trailer: value\r\n",
    ] {
        let transport = Transport::new(chunked_response(encoded));
        assert!(
            block_on(opened(&transport).finish()).is_err(),
            "{encoded:?}"
        );
    }
}

#[test]
fn finish_rejects_ambiguous_and_unsupported_transfer_encodings() {
    for headers in [
        "Transfer-Encoding: chunked\r\nContent-Length: 2\r\n",
        "Content-Length: 2\r\nTransfer-Encoding: chunked\r\n",
        "Transfer-Encoding: gzip, chunked\r\n",
        "Transfer-Encoding: chunked, gzip\r\n",
        "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
        "Transfer-Encoding: \r\n",
    ] {
        let transport =
            Transport::new(format!("HTTP/1.1 200 OK\r\n{headers}\r\n0\r\n\r\n").into_bytes());
        assert!(matches!(
            block_on(opened(&transport).finish()),
            Err(Error::HttpParseError)
        ));
    }
}
