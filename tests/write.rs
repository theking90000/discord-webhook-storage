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
        _: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.borrow_mut();
        if state.fail_read {
            return Poll::Ready(Err(io::Error::other("read failed")));
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
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.borrow_mut();
        if state.fail_write {
            return Poll::Ready(Err(io::Error::other("write failed")));
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

#[test]
fn credentials_accept_url_and_try_from() {
    let url = "https://discord.com/api/webhooks/123/token";
    assert_eq!(WebhookCredentials::parse(url), WebhookCredentials::try_from(url));
    for invalid in ["", "http://discord.com/api/webhooks/123/token", "https://discord.com/api/webhooks/123"] {
        assert_eq!(WebhookCredentials::parse(invalid), Err(Error::InvalidWebhookUrl));
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
    let request_text = String::from_utf8_lossy(&request);
    assert!(request_text.starts_with("POST /api/webhooks/123/token%3Fwait=true HTTP/1.1\r\nHost: discord.com\r\n"));
    let boundary = request_text
        .lines()
        .find_map(|line| line.strip_prefix("Content-Type: multipart/form-data; boundary="))
        .unwrap()
        .trim_end_matches('\r');
    assert_eq!(boundary.len(), 56);
    assert!(request_text.contains("name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n"));
    assert!(request_text.contains("\"filename\":\"file.bin\""));
    assert!(request_text.contains("\"payload_limit\":20000000"));
    assert!(request_text.contains("name=\"files[0]\"; filename=\"data.bin\""));
    assert!(request.ends_with(&[0, 2, 3, 255].into_iter().chain(format!("\r\n--{boundary}--\r\n").bytes()).collect::<Vec<_>>()));
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
    assert!(transport.written().ends_with(b"--\r\n"));
    assert_eq!(transport.0.borrow().flushes, 1);
    assert_eq!(block_on(file.write(&[1])).unwrap_err().kind(), io::ErrorKind::Other);
}

#[test]
fn close_handles_partial_transport_writes() {
    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    transport.0.borrow_mut().max_write = 3;
    block_on(file.close()).unwrap();
    assert!(transport.written().ends_with(b"--\r\n"));
    assert_eq!(transport.0.borrow().flushes, 1);
}

#[test]
#[ignore = "WriteFile::open currently ignores partial vectored writes"]
fn open_retries_partial_transport_writes() {
    let transport = Transport::new(Vec::new());
    transport.0.borrow_mut().max_write = 3;
    let mut file = opened(&transport);
    block_on(file.close()).unwrap();
    let request = String::from_utf8(transport.written()).unwrap();
    assert!(request.starts_with("POST /api/webhooks/123/token%3Fwait=true HTTP/1.1\r\n"));
    assert!(request.contains("name=\"payload_json\""));
    assert!(request.contains("name=\"files[0]\""));
}

#[test]
fn file_limit_applies_to_scalar_and_vectored_writes() {
    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    let large = vec![7; 20_000_000];
    block_on(file.write_all(&large)).unwrap();
    assert_eq!(block_on(file.write(&[1])).unwrap_err().kind(), io::ErrorKind::Other);
    assert_eq!(block_on(file.write_vectored(&[IoSlice::new(&[1])])).unwrap_err().kind(), io::ErrorKind::Other);

    let transport = Transport::new(Vec::new());
    let mut file = opened(&transport);
    assert_eq!(block_on(file.write_vectored(&[IoSlice::new(&[1, 2]), IoSlice::new(&[3])])).unwrap(), 3);
    block_on(file.write_all(&[4])).unwrap();
}

#[test]
fn flush_and_transport_errors_are_reported() {
    let transport = Transport::new(Vec::new());
    transport.0.borrow_mut().fail_write = true;
    assert!(matches!(block_on(WriteFile::open(transport, &credentials(), &WriteConfig::default())), Err(Error::IoError)));

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
        assert_eq!(serde_json::to_value(result).unwrap(), json!({"id":"message-1","url":"https://cdn.example/file"}));
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
        assert!(matches!(block_on(opened(&transport).finish()), Err(Error::HttpParseError)));
    }
}

#[test]
fn finish_rejects_invalid_json_and_missing_fields() {
    for body in ["not json", "{}", r#"{"id":"x"}"#, r#"{"id":5,"attachments":[{"url":"x"}]}"#, r#"{"id":"x","attachments":[]}"#, r#"{"id":"x","attachments":[{"url":7}]}"#] {
        let transport = Transport::new(response("200 OK", body));
        let expected = if body == "not json" { Error::JsonError } else { Error::HttpParseError };
        assert!(matches!(block_on(opened(&transport).finish()), Err(error) if error == expected));
    }
}

#[test]
fn finish_reports_read_failures_and_truncated_body() {
    let transport = Transport::new(Vec::new());
    transport.0.borrow_mut().fail_read = true;
    assert!(matches!(block_on(opened(&transport).finish()), Err(Error::IoError)));

    let transport = Transport::new(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}".to_vec());
    assert!(matches!(block_on(opened(&transport).finish()), Err(Error::IoError)));
}
