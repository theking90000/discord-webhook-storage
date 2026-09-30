use std::{
    io::IoSlice,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    Error, ResponseError, Result, WebhookCredentials, WriteError, chunk_writer::ChunkWriter,
    http::HttpStatusParser,
};
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt};
use rand::{RngExt, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// Default upload limit enforced by this crate, excluding multipart framing.
const DISCORD_PAYLOAD_LIMIT: usize = 20_000_000;

// Bound the decoded JSON response before allocating its body.
const MAX_RESPONSE_BODYSIZE: usize = 16384;

/// An open file upload implementing [`AsyncWrite`].
///
/// Each write sends file bytes in an HTTP chunk. Writes that exceed the remaining
/// payload allowance fail before sending any bytes from that call with
/// [`std::io::ErrorKind::InvalidInput`]. Writes after closing starts return
/// [`std::io::ErrorKind::BrokenPipe`]. Both retain a typed [`Error::WriteError`]
/// available through [`std::io::Error::get_ref`].
/// [`AsyncWriteExt::flush`] drains buffered bytes without finishing the upload.
/// [`AsyncWriteExt::close`] finishes the request body and keeps the transport open
/// so that [`Self::finish`] can read the response.
///
/// Call [`Self::finish`] to obtain the message identifier and attachment URL.
/// Dropping the writer does not complete the upload.
pub struct WriteFile<T> {
    connection: ChunkWriter<T>,
    boundary: String,

    state: WriteFileState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WriteFileState {
    /// Remaining file payload allowance.
    Writing(usize),
    /// Write offset and bytes of the multipart closing boundary.
    Closing(usize, Vec<u8>),
    /// Finish chunk framing and flush the transport.
    Finalizing,
    /// The request body is complete; the response has not yet been read.
    Closed,
}

/// The message identifier and first attachment URL returned by an upload.
///
/// Serialize this value with Serde to persist its `id` and `url` fields.
/// The fields are private; this crate currently provides no download operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WrittenFile {
    /// Identifier of the webhook message containing the attachment.
    id: String,
    /// Attachment URL from the webhook response.
    url: String,
}

/// Upload settings used by [`WriteFile::open`].
///
/// [`Default`] uses the filename `file.bin` and a payload limit of 20,000,000 bytes.
/// The fields are private and currently have no public setters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteConfig {
    /// Filename sent in the JSON metadata and multipart headers.
    filename: String,
    /// Maximum number of file bytes accepted by the writer.
    payload_limit: usize,
}

impl WriteConfig {
    fn as_request(&self) -> Value {
        json!({
            "attachments": [{
                "id": 0,
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
        format!($($arg)*).as_bytes()
    };
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
    /// Start an upload using an established TLS connection to `discord.com:443`.
    ///
    /// Sends the HTTP request headers, JSON metadata, and multipart file headers.
    /// The caller supplies a transport implementing the `futures` I/O traits.
    /// This method does not establish a connection or perform a TLS handshake.
    ///
    /// `credentials` selects the webhook, and `config` sets the file metadata and
    /// payload allowance. The writer owns the connection after this call.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::IoError`] for transport failures or
    /// [`crate::Error::JsonError`] if serializing the request metadata fails.
    pub async fn open(
        mut connection: T,
        credentials: &WebhookCredentials<'_>,
        config: &WriteConfig,
    ) -> Result<Self> {
        let boundary = generate_boundary();
        // Send headers before enabling chunk framing for the request body.
        connection
            .write_all(format_bytes!(
                "POST /api/webhooks/{}/{}?wait=true HTTP/1.1\r\n\
            Host: discord.com\r\n\
            Transfer-Encoding: chunked\r\n\
            Content-Type: multipart/form-data; boundary={}\r\n\
            \r\n",
                credentials.id,
                credentials.token,
                &boundary
            ))
            .await?;

        let mut connection = ChunkWriter::new(connection);

        connection
            .write_all(format_bytes!(
                "--{}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\n\
        Content-Type: application/json\r\n\r\n\
        {}\r\n",
                &boundary,
                &serde_json::to_string(&config.as_request())?
            ))
            .await?;

        connection
            .write_all(format_bytes!(
                "--{}\r\n\
                Content-Disposition: form-data; name=\"files[0]\"; filename=\"{}\"\r\n\r\n",
                &boundary,
                &config.filename
            ))
            .await?;

        Ok(Self {
            connection,
            boundary,
            state: WriteFileState::Writing(config.payload_limit),
        })
    }

    fn remaining_write(&self, buf_len: usize) -> std::io::Result<usize> {
        match self.state {
            WriteFileState::Writing(n) => {
                if buf_len <= n {
                    Ok(n)
                } else {
                    Err(WriteError::PayloadLimitExceeded { remaining: n, attempted: buf_len }.into())
                }
            }
            _ => Err(WriteError::NotWritable.into()),
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> WriteFile<T> {
    /// Complete the upload and read its message identifier and attachment URL.
    ///
    /// Closes the request body if needed, then reads an HTTP/1.1 response with
    /// `Content-Length` or chunked transfer encoding. The decoded response body
    /// must fit within 16 KiB. This consumes the writer and its connection.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::IoError`] for transport failures and
    /// [`crate::Error::JsonError`] for invalid response JSON.
    /// [`crate::Error::HttpParseError`] details malformed HTTP, unsupported framing,
    /// and response size limits. [`crate::Error::HttpStatus`] retains unsuccessful
    /// statuses and their decoded response bodies. [`crate::Error::InvalidResponse`]
    /// identifies missing or incorrectly typed JSON fields.
    pub async fn finish(mut self) -> Result<WrittenFile> {
        // Send the multipart boundary and final chunk before reading the response.
        self.close().await?;

        let mut parser = HttpStatusParser::new(&mut self.connection);
        let status = parser.status().await?;
        let mut parser = parser.into_headers();
        while !parser.is_complete() {
            parser.next_header().await?;
        }
        let body = parser.body(MAX_RESPONSE_BODYSIZE).await?;

        if status != 200 {
            return Err(Error::HttpStatus { status, body });
        }

        let json = serde_json::from_slice::<Value>(&body)?;

        if !json.is_object() {
            return Err(ResponseError::InvalidFieldType {
                field: "$",
                expected: "object",
                actual: json,
            }.into());
        }
        let id = response_field(&json, "id", "id")?;
        let id = response_string(id, "id")?;
        let attachments = response_field(&json, "attachments", "attachments")?;
        let attachments = attachments.as_array().ok_or_else(|| ResponseError::InvalidFieldType {
            field: "attachments",
            expected: "array",
            actual: attachments.clone(),
        })?;
        let attachment = attachments.first().ok_or(ResponseError::MissingField {
            field: "attachments[0]",
        })?;
        if !attachment.is_object() {
            return Err(ResponseError::InvalidFieldType {
                field: "attachments[0]",
                expected: "object",
                actual: attachment.clone(),
            }.into());
        }
        let url = response_field(attachment, "url", "attachments[0].url")?;
        let url = response_string(url, "attachments[0].url")?;
        Ok(WrittenFile {
            id: id.to_string(),
            url: url.to_string(),
        })
    }
}

fn response_field<'a>(value: &'a Value, key: &str, field: &'static str) -> Result<&'a Value> {
    value.get(key).ok_or_else(|| ResponseError::MissingField { field }.into())
}

fn response_string<'a>(value: &'a Value, field: &'static str) -> Result<&'a str> {
    value.as_str().ok_or_else(|| ResponseError::InvalidFieldType {
        field,
        expected: "string",
        actual: value.clone(),
    }.into())
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
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();

        let Some(buf_len) = bufs.iter().try_fold(0usize, |n, buf| n.checked_add(buf.len())) else {
            return Poll::Ready(Err(WriteError::SizeOverflow.into()));
        };
        let remaining = match this.remaining_write(buf_len) {
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
                    let buf = format!("\r\n--{}--\r\n", this.boundary).into_bytes();
                    this.state = WriteFileState::Closing(0, buf);
                }
                WriteFileState::Closing(n, buf) => {
                    match Pin::new(&mut this.connection).poll_write(cx, &buf[*n..]) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(0)) => {
                            return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
                        }
                        Poll::Ready(Ok(nr)) => {
                            if (nr + (*n)) == buf.len() {
                                this.state = WriteFileState::Finalizing;
                            } else {
                                // Preserve the offset across partial writes and Pending.
                                *n += nr;
                            }
                        }
                    }
                }
                // Finish HTTP chunk framing after the multipart closing boundary.
                // ChunkWriter::poll_close keeps the transport open for the response.
                WriteFileState::Finalizing => match Pin::new(&mut this.connection).poll_close(cx) {
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
