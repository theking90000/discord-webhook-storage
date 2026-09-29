use std::{
    pin::Pin,
    task::{Context, Poll},
};

use crate::{Error::HttpParseError, Result, WebhookCredentials, http::HttpStatusParser};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use rand::{RngExt, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// Discord limits to exactly 20MB the file payload
// as of 29 september 2026 (might change in the future)
const DISCORD_PAYLOAD_LIMIT: usize = 20_000_000;

// Do not read the response if the body response size
// is more than 16kB. This is a safeguard to avoid memory over-usage
// and OS crashes because of memory allocations
const MAX_RESPONSE_BODYSIZE: usize = 16384;

/// Represent a writable open file
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteFile<T> {
    connection: T,
    boundary: String,

    state: WriteFileState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WriteFileState {
    /// number of bytes allowed remaining
    Writing(usize),
    /// closing boundary
    Closing(usize, Vec<u8>),
    Flushing,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WrittenFile {
    id: String,
    url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteConfig {
    filename: String,
    payload_limit: usize,
}

impl WriteConfig {
    fn as_request(&self) -> Value {
        json!({
            "attachments": [{
                "id": 0,
                //"description": "File",
                "filename": &self.filename,
                "payload_limit": &self.payload_limit,
            }]
        })
    }
}

impl Default for WriteConfig {
    fn default() -> Self {
        WriteConfig {
            filename: "file.bin".into(),
            payload_limit: DISCORD_PAYLOAD_LIMIT,
        }
    }
}

macro_rules! format_bytes {
    ($($arg:tt)*) => {
        format!($($arg)*).into_bytes()
    };
}

macro_rules! write_vectored {
    ($writer:expr, [$($buf:expr),* $(,)?]) => {{
        let buffers = [
            $($buf),*
        ];

        let bufs = buffers
            .each_ref()
            .map(|buf| std::io::IoSlice::new(buf));

        $writer.write_vectored(&bufs).await
    }};
}

fn generate_boundary() -> String {
    let random: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();

    format!("------------------------{random}")
}

impl<T: AsyncWrite + Unpin> WriteFile<T> {
    /// Open a new writable file to a Discord webhook.
    /// * `connection` - Holds an open TCP/TLS connection to discord.com:443
    /// * `credentials` - the webhook credentials used for this request
    /// * `config` - the custom webhook configuration
    pub async fn open(
        mut connection: T,
        credentials: &WebhookCredentials<'_>,
        config: &WriteConfig,
    ) -> Result<Self> {
        let boundary = generate_boundary();

        write_vectored!(
            connection,
            [
                format_bytes!(
                    "POST /api/webhooks/{}/{}%3Fwait=true HTTP/1.1\r\n",
                    credentials.id,
                    credentials.token
                ),
                format_bytes!("Host: discord.com\r\n"),
                format_bytes!(
                    "Content-Type: multipart/form-data; boundary={}\r\n",
                    &boundary
                ),
                format_bytes!("\r\n"),
            ]
        )?;

        write_vectored!(
            connection,
            [
                format_bytes!("--{}\r\n", boundary),
                format_bytes!("Content-Disposition: form-data; name=\"payload_json\"\r\n"),
                format_bytes!("Content-Type: application/json\r\n\r\n"),
                serde_json::to_vec(&config.as_request())?,
                format_bytes!("\r\n"),
            ]
        )?;

        write_vectored!(
            connection,
            [
                format_bytes!("--{}\r\n", &boundary),
                format_bytes!(
                    "Content-Disposition: form-data; name=\"files[0]\"; filename=\"data.bin\"\r\n\r\n"
                )
            ]
        )?;

        Ok(Self {
            connection,
            boundary,
            // from config OR default?
            state: WriteFileState::Writing(config.payload_limit),
        })
    }

    fn remaining_write(&self, buf_len: usize) -> std::io::Result<usize> {
        match self.state {
            WriteFileState::Writing(n) => {
                if buf_len <= n {
                    Ok(n)
                } else {
                    Err(std::io::Error::other("file is not writable"))
                }
            }
            _ => Err(std::io::Error::other("file is not writable")),
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> WriteFile<T> {
    /// Read the server HTTP response
    /// And
    pub async fn finish(mut self) -> Result<WrittenFile> {
        // ensure we are in a closed state.
        self.close().await?;

        let mut parser = HttpStatusParser::new();
        // buf used to parse headers.
        let mut buf = [0; 1024];

        let status = loop {
            if let Some(status) = parser.status()? {
                break status;
            }

            let n = self.connection.read(&mut buf).await?;
            if n == 0 {
                return Err(HttpParseError);
            }

            parser.feed(&buf[..n]);
        };

        let mut parser = parser.into_headers();

        loop {
            while let Some((_name, _value)) = parser.next_header()? {
                // process headers ; principally rate limit and stuff
            }

            if parser.is_complete() {
                break;
            }

            let n = self.connection.read(&mut buf).await?;
            if n == 0 {
                return Err(HttpParseError);
            }

            parser.feed(&buf[..n]);
        }

        if status != 200 {
            // TODO: bette handle Error taxonomy
            return Err(HttpParseError);
        }

        let body_size = parser.body_size().ok_or(HttpParseError)?;

        if body_size > MAX_RESPONSE_BODYSIZE {
            return Err(HttpParseError);
        }

        let mut body = parser.remaining();

        if body.len() > body_size {
            return Err(HttpParseError);
        }

        let already_read = body.len();

        body.resize(body_size, 0);

        self.connection
            .read_exact(&mut body[already_read..])
            .await?;

        // body contains exactly our JSON payload
        let json = serde_json::from_slice::<Value>(&body)?;

        let id = json
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or(HttpParseError)?;

        let url = json
            .get("attachments")
            .and_then(|v| v.as_array())
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
            .ok_or(HttpParseError)?;

        Ok(WrittenFile {
            id: id.to_string(),
            url: url.to_string(),
        })
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteFile<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();

        let remaining = match this.remaining_write(buf.len()) {
            Ok(n) => n,
            Err(e) => return Poll::Ready(Err(e)),
        };

        let result = Pin::new(&mut this.connection).poll_write(cx, buf);

        if let Poll::Ready(Ok(n)) = result {
            this.state = WriteFileState::Writing(remaining - n);
        }

        result
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let total_len: usize = bufs.iter().map(|buf| buf.len()).sum();

        let remaining = match this.remaining_write(total_len) {
            Ok(n) => n,
            Err(e) => return Poll::Ready(Err(e)),
        };

        let result = Pin::new(&mut this.connection).poll_write_vectored(cx, bufs);

        if let Poll::Ready(Ok(n)) = result {
            this.state = WriteFileState::Writing(remaining - n);
        }

        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        Pin::new(&mut this.connection).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        loop {
            match &mut this.state {
                WriteFileState::Writing(_) => {
                    let buf = format_bytes!("\r\n--{}--\r\n", &this.boundary);
                    this.state = WriteFileState::Closing(0, buf);
                }
                WriteFileState::Closing(n, buf) => {
                    match Pin::new(&mut this.connection).poll_write(cx, &buf[*n..]) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(nr)) => {
                            if (nr + (*n)) == buf.len() {
                                this.state = WriteFileState::Flushing;
                                // return Poll::Ready(Ok(()));
                            } else {
                                // advance Vec pointer by nr
                                *n += nr;
                            }
                        }
                    }
                }
                WriteFileState::Flushing => match Pin::new(&mut this.connection).poll_flush(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {
                        this.state = WriteFileState::Closed;
                        return Poll::Ready(Ok(()));
                    }
                },
                WriteFileState::Closed => return Poll::Ready(Ok(())),
            }
        }
    }
}
