use std::{
    io::IoSlice,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    DiscordFile, DiscordFileUrl, Error, ResponseError, Result, WebhookCredentials, WriteError,
    chunk_writer::ChunkWriter, http::HttpStatusParser,
};
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt};
use rand::{RngExt, distr::Alphanumeric};
use serde_json::{Value, json};

// Default upload limit enforced by this crate, excluding multipart framing.
const DISCORD_PAYLOAD_LIMIT: usize = 20_000_000;

// Bound the decoded JSON response before allocating its body.
const MAX_RESPONSE_BODYSIZE: usize = 16384;

/// A file opened for asynchronous writing to Discord.
///
/// Write its contents with the standard [`AsyncWrite`] operations, then call
/// [`Self::finish`] to confirm that Discord stored the file and obtain its
/// reference. Writing all bytes or calling [`AsyncWriteExt::close`] alone does
/// not confirm success. Dropping the file does not complete the upload.
///
/// [`AsyncWriteExt::flush`] sends any buffered bytes without finishing the file.
/// [`AsyncWriteExt::close`] ends writing, after which [`Self::finish`] can still
/// confirm the upload. A write that would exceed the file size limit fails with
/// [`std::io::ErrorKind::InvalidInput`] without accepting any bytes from that
/// call. Writing after closing starts returns [`std::io::ErrorKind::BrokenPipe`].
/// See [`WriteError`] for details on these write failures.
///
/// The caller supplies a secure connection to `discord.com:443` using HTTP/1.1.
/// Pass `&mut connection` to keep ownership of it. Reuse it only after
/// [`Self::finish`] succeeds and if it is still open. Discard it after an error
/// or an interrupted upload. Passing a connection by value makes the file own
/// it and drop it when the file is finished or dropped.
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

/// Settings for creating a file with [`WriteFile::open`].
///
/// Use [`WriteConfig::default`] for a file named `file.bin` with a maximum size
/// of 20,000,000 bytes. These settings cannot currently be customized.
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
    /// Create a new file for writing through the selected Discord webhook.
    ///
    /// `credentials` selects where to store the file. `config` specifies its
    /// name and maximum size. Discord assigns the file reference when
    /// [`Self::finish`] succeeds.
    ///
    /// Supply an already connected secure connection to `discord.com:443` as
    /// described in [`WriteFile`]. Pass `&mut connection` to keep ownership of it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IoError`] if the connection fails, or
    /// [`Error::JsonError`] if the file settings cannot be sent to Discord.
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
                    Err(WriteError::PayloadLimitExceeded {
                        remaining: n,
                        attempted: buf_len,
                    }
                    .into())
                }
            }
            _ => Err(WriteError::NotWritable.into()),
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> WriteFile<T> {
    /// Finish writing and return a reference to the stored file.
    ///
    /// Success confirms that Discord accepted the complete file. Keep the
    /// returned [`DiscordFile`] to open it for reading later. It supports Serde
    /// serialization, though its download URL expires.
    ///
    /// This consumes the writer and closes it if needed. If opening received
    /// `&mut connection`, the connection becomes available for reuse on success,
    /// if it is still open. A connection passed by value is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IoError`] if the connection fails, or
    /// [`Error::HttpStatus`] if Discord rejects the upload.
    ///
    /// An unreadable response returns [`Error::HttpParseError`] or
    /// [`Error::JsonError`]. Missing or invalid file details return
    /// [`Error::InvalidResponse`] or [`Error::InvalidDiscordFileUrl`]. The
    /// response is limited to 16 KiB.
    ///
    /// Discard a retained connection after any error.
    pub async fn finish(mut self) -> Result<DiscordFile> {
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
            }
            .into());
        }
        let id = response_field(&json, "id", "id")?;
        let id = response_string(id, "id")?;
        let attachments = response_field(&json, "attachments", "attachments")?;
        let attachments =
            attachments
                .as_array()
                .ok_or_else(|| ResponseError::InvalidFieldType {
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
            }
            .into());
        }
        let url = response_field(attachment, "url", "attachments[0].url")?;
        let url = response_string(url, "attachments[0].url")?;
        Ok(DiscordFile {
            id: id.to_string(),
            url: DiscordFileUrl::parse(url)?,
        })
    }
}

fn response_field<'a>(value: &'a Value, key: &str, field: &'static str) -> Result<&'a Value> {
    value
        .get(key)
        .ok_or_else(|| ResponseError::MissingField { field }.into())
}

fn response_string<'a>(value: &'a Value, field: &'static str) -> Result<&'a str> {
    value.as_str().ok_or_else(|| {
        ResponseError::InvalidFieldType {
            field,
            expected: "string",
            actual: value.clone(),
        }
        .into()
    })
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

        let Some(buf_len) = bufs
            .iter()
            .try_fold(0usize, |n, buf| n.checked_add(buf.len()))
        else {
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
